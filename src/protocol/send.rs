use crate::protocol::progress::{
    EmitThrottle, ShareProgress, SpeedMeter, TransferClock, PROGRESS_MIN_BYTES, PROGRESS_MIN_SECS,
    SPEED_WINDOW_SECS,
};
use crate::protocol::time_compat::{sleep, Duration, Instant};
use crate::protocol::types::AppHandle;
use iroh::EndpointId;
use iroh_blobs::protocol::ChunkRangesSeq;
use iroh_blobs::provider::events::RequestUpdate;
use n0_future::{task::AbortOnDropHandle, StreamExt};
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use tokio::sync::mpsc;

/// What a share's progress tracker hears from the blob provider, already
/// narrowed down to that one share.
pub enum ShareEvent {
    /// A peer opened a connection to fetch this share.
    PeerConnected(EndpointId),
    /// A get request for this share; `updates` streams its transfer events.
    /// Dropping `updates` aborts the transfer at its next chunk.
    Request {
        connection_id: u64,
        request_id: u64,
        ranges: ChunkRangesSeq,
        updates: irpc::channel::mpsc::Receiver<RequestUpdate>,
    },
}

fn emit_event(app_handle: &AppHandle, event_name: &str) {
    if let Some(handle) = app_handle {
        if let Err(e) = handle.emit_event(event_name) {
            tracing::warn!("Failed to emit event {}: {}", event_name, e);
        }
    }
}

fn emit_progress_event(
    app_handle: &AppHandle,
    bytes_transferred: u64,
    total_size: u64,
    speed_bps: f64,
) {
    if let Some(handle) = app_handle {
        let event_name = "transfer-progress";

        let payload = crate::protocol::progress::format_progress_payload(bytes_transferred, total_size, speed_bps);
        if let Err(e) = handle.emit_event_with_payload(event_name, &payload) {
            tracing::warn!("Failed to emit progress event: {}", e);
        }
    }
}

fn emit_active_connection_count(app_handle: &AppHandle, count: usize) {
    if let Some(handle) = app_handle {
        let event_name = "active-connection-count";
        let payload = count.to_string();

        if let Err(e) = handle.emit_event_with_payload(event_name, &payload) {
            tracing::warn!("Failed to emit active connection count event: {}", e);
        }
    }
}

/// Spawns the task that turns a share's [`ShareEvent`]s into progress events
/// on `app_handle`. Aborting the task drops every request's update stream,
/// which aborts the transfers still running.
pub fn spawn_share_progress(
    events: mpsc::Receiver<ShareEvent>,
    app_handle: AppHandle,
    size: u64,
    completed_peers: Arc<AtomicUsize>,
) -> AbortOnDropHandle<anyhow::Result<()>> {
    AbortOnDropHandle::new(n0_future::task::spawn(show_provide_progress_with_logging(
        events,
        app_handle,
        size,
        completed_peers,
    )))
}

/// Range specs used by receivers before the main payload download (hash-seq + child sizes).
fn is_sizes_probe_request(ranges: &iroh_blobs::protocol::ChunkRangesSeq) -> bool {
    use iroh_blobs::protocol::{ChunkRanges, ChunkRangesExt, ChunkRangesSeq};

    ranges == &ChunkRangesSeq::verified_child_sizes()
        || ranges
            == &ChunkRangesSeq::from_ranges_infinite([ChunkRanges::all(), ChunkRanges::last_chunk()])
}

/// Only treat a request as the final payload transfer once nearly all bytes were sent.
fn transfer_payload_complete(bytes_sent: u64, total_size: u64) -> bool {
    if total_size == 0 {
        return true;
    }
    // Size probes only fetch hash-seq headers and last chunks, far below payload size.
    bytes_sent.saturating_mul(100) >= total_size.saturating_mul(95)
}

/// Session-wide progress state shared by every request of one share.
struct SessionProgress {
    ledger: ShareProgress,
    speed: SpeedMeter,
    throttle: EmitThrottle,
    clock: TransferClock,
    start: Instant,
}

impl SessionProgress {
    fn new(share_size: u64) -> Self {
        Self {
            ledger: ShareProgress::new(share_size),
            speed: SpeedMeter::new(SPEED_WINDOW_SECS),
            throttle: EmitThrottle::new(PROGRESS_MIN_BYTES, PROGRESS_MIN_SECS),
            clock: TransferClock::new(),
            start: Instant::now(),
        }
    }

    fn now_secs(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }

    /// Record the aggregate byte count, returning `true` when it is due to be emitted.
    fn record(&mut self, at_secs: f64, bytes: u64) -> bool {
        self.clock.mark_activity(at_secs);
        self.speed.record(at_secs, bytes);
        self.throttle.should_emit(at_secs, bytes)
    }

    /// Time spent serving the transfer, excluding connection setup and the
    /// completion debounce.
    fn wire_duration_ms(&self) -> u64 {
        self.clock.duration_ms()
    }
}

fn emit_session_progress(app_handle: &AppHandle, session: &SessionProgress, at_secs: f64) {
    let snapshot = session.ledger.snapshot();
    emit_progress_event(
        app_handle,
        snapshot.bytes,
        snapshot.total,
        session.speed.bytes_per_sec_at(at_secs),
    );
}

fn emit_transfer_completed(app_handle: &AppHandle, session: &SessionProgress) {
    let snapshot = session.ledger.snapshot();
    let payload = serde_json::json!({
        "durationMs": session.wire_duration_ms(),
        "bytes": snapshot.bytes,
        "totalBytes": snapshot.total,
    });
    if let Some(handle) = app_handle {
        if let Err(e) = handle.emit_event_with_payload("transfer-completed", &payload.to_string()) {
            tracing::warn!("Failed to emit transfer-completed: {}", e);
        }
    }
}

async fn show_provide_progress_with_logging(
    mut recv: mpsc::Receiver<ShareEvent>,
    app_handle: AppHandle,
    total_collection_size: u64,
    completed_requests: Arc<AtomicUsize>,
) -> anyhow::Result<()> {
    use n0_future::FuturesUnordered;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    let mut tasks = FuturesUnordered::new();

    let session: Arc<Mutex<SessionProgress>> =
        Arc::new(Mutex::new(SessionProgress::new(total_collection_size)));

    let active_requests = Arc::new(AtomicUsize::new(0));
    let has_emitted_started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let has_emitted_completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let last_request_time: Arc<tokio::sync::Mutex<Option<Instant>>> =
        Arc::new(tokio::sync::Mutex::new(None));

    loop {
        tokio::select! {
            biased;
            item = recv.recv() => {
                let Some(item) = item else {
                    break;
                };

                match item {
                    ShareEvent::PeerConnected(endpoint_id) => {
                        let payload = serde_json::json!({
                            "endpoint_id": endpoint_id.to_string(),
                        });
                        if let Some(handle) = &app_handle {
                            let _ = handle.emit_event_with_payload(
                                "share-peer-connected",
                                &payload.to_string(),
                            );
                        }
                    }
                    ShareEvent::Request { connection_id, request_id, ranges, updates } => {
                        let is_sizes_probe_request = is_sizes_probe_request(&ranges);

                        let key = (connection_id, request_id);

                        if !is_sizes_probe_request {
                            active_requests.fetch_add(1, Ordering::SeqCst);

                            let mut last_time = last_request_time.lock().await;
                            *last_time = Some(Instant::now());
                        }

                        let app_handle_task = app_handle.clone();
                        let session_task = session.clone();
                        let active_requests_task = active_requests.clone();
                        let completed_requests_task = completed_requests.clone();
                        let has_emitted_started_task = has_emitted_started.clone();
                        let has_emitted_completed_task = has_emitted_completed.clone();
                        let last_request_time_task = last_request_time.clone();
                        let total_collection_size_task = total_collection_size;

                        let mut rx = updates;
                        tasks.push(async move {
                            if is_sizes_probe_request {
                                while let Ok(Some(_)) = rx.recv().await {}
                                return;
                            }

                            let mut transfer_started = false;
                            let mut request_completed = false;

                            while let Ok(Some(update)) = rx.recv().await {
                                match update {
                                    RequestUpdate::Started(m) => {
                                        let active_count = {
                                            let mut session = session_task.lock().await;
                                            let at_secs = session.now_secs();
                                            session.clock.mark_start(at_secs);
                                            session.ledger.blob_started(key, m.index, m.size);
                                            session.ledger.active_requests()
                                        };

                                        if !transfer_started {
                                            emit_active_connection_count(&app_handle_task, active_count);

                                            if !has_emitted_started_task.swap(true, Ordering::SeqCst) {
                                                emit_event(&app_handle_task, "transfer-started");
                                            }

                                            transfer_started = true;
                                        }
                                    }
                                    RequestUpdate::Progress(m) => {
                                        if !transfer_started {
                                            let active_count = {
                                                let mut session = session_task.lock().await;
                                                let at_secs = session.now_secs();
                                                session.clock.mark_start(at_secs);
                                                session.ledger.ensure_request(key);
                                                session.ledger.active_requests()
                                            };

                                            emit_active_connection_count(&app_handle_task, active_count);

                                            if !has_emitted_started_task.swap(true, Ordering::SeqCst) {
                                                emit_event(&app_handle_task, "transfer-started");
                                            }
                                            transfer_started = true;
                                        }

                                        let mut session = session_task.lock().await;
                                        session.ledger.blob_progress(key, m.end_offset);
                                        let at_secs = session.now_secs();
                                        let bytes = session.ledger.snapshot().bytes;
                                        if session.record(at_secs, bytes) {
                                            emit_session_progress(&app_handle_task, &session, at_secs);
                                        }
                                    }
                                    RequestUpdate::Completed(_m) => {
                                        if transfer_started && !request_completed {
                                            let (bytes_sent, active_count) = {
                                                let mut session = session_task.lock().await;
                                                let bytes_sent = session.ledger.request_bytes(key);
                                                // A peer that stopped far short of the payload
                                                // is dropped from the totals rather than
                                                // holding the share below 100% forever.
                                                let credited = transfer_payload_complete(
                                                    bytes_sent,
                                                    total_collection_size_task,
                                                );
                                                session.ledger.retire(key, credited);
                                                let at_secs = session.now_secs();
                                                session.clock.mark_activity(at_secs);
                                                emit_session_progress(&app_handle_task, &session, at_secs);
                                                (bytes_sent, session.ledger.active_requests())
                                            };

                                            emit_active_connection_count(&app_handle_task, active_count);

                                            request_completed = true;

                                            if !transfer_payload_complete(
                                                bytes_sent,
                                                total_collection_size_task,
                                            ) {
                                                active_requests_task.fetch_sub(1, Ordering::SeqCst);
                                                continue;
                                            }

                                            let completed = completed_requests_task.fetch_add(1, Ordering::SeqCst) + 1;
                                            let active = active_requests_task.load(Ordering::SeqCst);

                                            // Size-probe requests are ignored above. A completed payload
                                            // request with all bytes sent indicates the end of the transfer.
                                            let min_required = 1;

                                            if completed >= active
                                                && completed >= min_required {
                                                let active_before_wait = active;

                                                sleep(Duration::from_millis(500)).await;

                                                let completed_after = completed_requests_task.load(Ordering::SeqCst);
                                                let active_after = active_requests_task.load(Ordering::SeqCst);

                                                let new_requests_arrived = active_after > active_before_wait;

                                                let has_active_transfers = {
                                                    let session = session_task.lock().await;
                                                    session.ledger.active_requests() > 0
                                                };

                                                let last_request_recent = {
                                                    let last_time = last_request_time_task.lock().await;
                                                    if let Some(time) = *last_time {
                                                        time.elapsed() < Duration::from_millis(500)
                                                    } else {
                                                        false
                                                    }
                                                };

                                                if completed_after >= active_after
                                                    && completed_after >= min_required
                                                    && !new_requests_arrived
                                                    && !has_active_transfers
                                                    && !last_request_recent {
                                                    if !has_emitted_completed_task
                                                        .swap(true, Ordering::SeqCst)
                                                    {
                                                        let session = session_task.lock().await;
                                                        emit_transfer_completed(&app_handle_task, &session);
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    RequestUpdate::Aborted(_m) => {
                                        tracing::warn!("Request aborted: conn {} req {}",
                                            connection_id, request_id);
                                        if transfer_started && !request_completed {
                                            let active_count = {
                                                let mut session = session_task.lock().await;
                                                let bytes_sent = session.ledger.request_bytes(key);
                                                let credited = transfer_payload_complete(
                                                    bytes_sent,
                                                    total_collection_size_task,
                                                );
                                                session.ledger.retire(key, credited);
                                                session.ledger.active_requests()
                                            };

                                            emit_active_connection_count(&app_handle_task, active_count);

                                            request_completed = true;

                                            let completed = completed_requests_task.fetch_add(1, Ordering::SeqCst) + 1;
                                            let active = active_requests_task.load(Ordering::SeqCst);

                                            if completed >= active {
                                                emit_event(&app_handle_task, "transfer-failed");
                                            }
                                        }
                                    }
                                }
                            }

                            if transfer_started && !request_completed {
                                let bytes_sent = {
                                    let mut session = session_task.lock().await;
                                    let bytes_sent = session.ledger.request_bytes(key);
                                    let credited = transfer_payload_complete(
                                        bytes_sent,
                                        total_collection_size_task,
                                    );
                                    session.ledger.retire(key, credited);
                                    bytes_sent
                                };

                                if !transfer_payload_complete(
                                    bytes_sent,
                                    total_collection_size_task,
                                ) {
                                    active_requests_task.fetch_sub(1, Ordering::SeqCst);
                                    return;
                                }

                                let completed = completed_requests_task.fetch_add(1, Ordering::SeqCst) + 1;
                                let active = active_requests_task.load(Ordering::SeqCst);

                                let min_required = 1;

                                if completed >= active
                                    && completed >= min_required {
                                    let active_before_wait = active;

                                    sleep(Duration::from_millis(500)).await;

                                    let completed_after = completed_requests_task.load(Ordering::SeqCst);
                                    let active_after = active_requests_task.load(Ordering::SeqCst);

                                    let new_requests_arrived = active_after > active_before_wait;

                                    let has_active_transfers = {
                                        let session = session_task.lock().await;
                                        session.ledger.active_requests() > 0
                                    };

                                    let last_request_recent = {
                                        let last_time = last_request_time_task.lock().await;
                                        if let Some(time) = *last_time {
                                            time.elapsed() < Duration::from_millis(500)
                                        } else {
                                            false
                                        }
                                    };

                                    if completed_after >= active_after
                                        && completed_after >= min_required
                                        && !new_requests_arrived
                                        && !has_active_transfers
                                        && !last_request_recent {
                                        if !has_emitted_completed_task
                                            .swap(true, Ordering::SeqCst)
                                        {
                                            let session = session_task.lock().await;
                                            emit_transfer_completed(&app_handle_task, &session);
                                        }
                                    }
                                }
                            }
                        });
                    }
                }
            }
            Some(_) = tasks.next(), if !tasks.is_empty() => {
            }
        }
    }

    while tasks.next().await.is_some() {}

    let completed = completed_requests.load(Ordering::SeqCst);
    let active = active_requests.load(Ordering::SeqCst);

    let min_required = 1;

    if completed >= active && completed >= min_required && completed > 0 {
        if !has_emitted_completed.swap(true, Ordering::SeqCst) {
            let session = session.lock().await;
            emit_transfer_completed(&app_handle, &session);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{is_sizes_probe_request, transfer_payload_complete};
    use iroh_blobs::protocol::{ChunkRanges, ChunkRangesExt, ChunkRangesSeq};

    #[test]
    fn sizes_probe_detection() {
        assert!(is_sizes_probe_request(&ChunkRangesSeq::verified_child_sizes()));
        assert!(is_sizes_probe_request(&ChunkRangesSeq::from_ranges_infinite([
            ChunkRanges::all(),
            ChunkRanges::last_chunk(),
        ])));
        assert!(!is_sizes_probe_request(&ChunkRangesSeq::all()));
    }

    #[test]
    fn payload_complete_threshold() {
        assert!(transfer_payload_complete(1000, 1000));
        assert!(transfer_payload_complete(950, 1000));
        assert!(!transfer_payload_complete(100, 1000));
        assert!(transfer_payload_complete(0, 0));
    }
}
