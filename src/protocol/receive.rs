use crate::protocol::progress::{
    duration_ms, split_child_sizes, EmitThrottle, SpeedMeter, PROGRESS_MIN_BYTES,
    PROGRESS_MIN_SECS, SPEED_WINDOW_SECS,
};
use crate::protocol::time_compat::Instant;
use crate::protocol::types::AppHandle;
use iroh::Endpoint;
use iroh_blobs::{
    api::{remote::GetProgressItem, Store},
    format::collection::Collection,
    get::{request::get_hash_seq_and_sizes, GetError, Stats},
    ticket::BlobTicket,
};
use n0_future::StreamExt;

// Helper function to emit events through the app handle
fn emit_event(app_handle: &AppHandle, event_name: &str) {
    if let Some(handle) = app_handle {
        if let Err(e) = handle.emit_event(event_name) {
            tracing::warn!("Failed to emit event {}: {}", event_name, e);
        }
    }
}

// Helper function to emit progress events with payload
fn emit_progress_event(
    app_handle: &AppHandle,
    bytes_transferred: u64,
    total_bytes: u64,
    speed_bps: f64,
) {
    if let Some(handle) = app_handle {
        let event_name = "receive-progress";

        let payload =
            crate::protocol::progress::format_progress_payload(bytes_transferred, total_bytes, speed_bps);

        // Emit the event with appropriate payload
        if let Err(e) = handle.emit_event_with_payload(event_name, &payload) {
            tracing::warn!("Failed to emit progress event: {}", e);
        }
    }
}

// Helper function to emit events with payload
fn emit_event_with_payload(app_handle: &AppHandle, event_name: &str, payload: &str) {
    if let Some(handle) = app_handle {
        if let Err(e) = handle.emit_event_with_payload(event_name, payload) {
            tracing::warn!("Failed to emit event {} with payload: {}", event_name, e);
        }
    }
}

pub struct DownloadToStoreResult {
    pub collection: Collection,
    pub total_files: u64,
    pub payload_size: u64,
    pub stats: Stats,
    /// Wire time for the payload, excluding connection setup and disk export.
    pub download_duration_ms: u64,
}

/// Download ticket payload into the blob store (no filesystem export).
pub async fn download_to_store(
    ticket: BlobTicket,
    addr: iroh::EndpointAddr,
    endpoint: &Endpoint,
    db: &Store,
    app_handle: &AppHandle,
) -> anyhow::Result<DownloadToStoreResult> {
    let hash_and_format = ticket.hash_and_format();
    let local = db.remote().local(hash_and_format).await?;

    let (stats, total_files, payload_size, download_duration_ms) = if !local.is_complete() {
        emit_event(app_handle, "receive-started");

        let connection = match endpoint
            .connect(addr.clone(), iroh_blobs::protocol::ALPN)
            .await
        {
            Ok(conn) => conn,
            Err(e) => {
                tracing::error!("Connection failed: {}", e);
                tracing::error!("Error details: {:?}", e);
                tracing::error!("Tried to connect to node: {}", addr.id);
                tracing::error!("With relay: {:?}", addr.relay_urls().collect::<Vec<_>>());
                tracing::error!(
                    "With direct addrs: {:?}",
                    addr.ip_addrs().collect::<Vec<_>>()
                );
                return Err(anyhow::anyhow!("Connection failed: {}", e));
            }
        };

        let sizes_result =
            get_hash_seq_and_sizes(&connection, &hash_and_format.hash, 1024 * 1024 * 32, None)
                .await;

        let (hash_seq, sizes) = match sizes_result {
            Ok((hash_seq, sizes)) => (hash_seq, sizes),
            Err(e) => {
                tracing::error!("Failed to get sizes: {:?}", e);
                tracing::error!("Error type: {}", std::any::type_name_of_val(&e));
                return Err(show_get_error(e).into());
            }
        };
        // `sizes` holds children only (entry 0 is the collection metadata), but
        // the get stream counts the hash-seq root and that blob too, subtract
        // them to leave the same "file bytes" the sender reports.
        let root_bytes = (hash_seq.len() as u64).saturating_mul(32);
        let split = split_child_sizes(root_bytes, &sizes);
        let payload_size = split.payload_bytes;
        let total_files = (sizes.len().saturating_sub(1)) as u64;

        emit_progress_event(app_handle, 0, payload_size, 0.0);

        let get = db.remote().execute_get(connection, local.missing());
        let mut stats = Stats::default();
        let mut stream = get.stream();
        let transfer_start_time = Instant::now();
        let mut speed = SpeedMeter::new(SPEED_WINDOW_SECS);
        let mut throttle = EmitThrottle::new(PROGRESS_MIN_BYTES, PROGRESS_MIN_SECS);
        let mut download_duration_ms = 0u64;

        while let Some(item) = stream.next().await {
            match item {
                GetProgressItem::Progress(offset) => {
                    let elapsed = transfer_start_time.elapsed().as_secs_f64();
                    let received = offset
                        .saturating_sub(split.overhead_bytes)
                        .min(payload_size);
                    speed.record(elapsed, received);
                    if throttle.should_emit(elapsed, received) {
                        emit_progress_event(
                            app_handle,
                            received,
                            payload_size,
                            speed.bytes_per_sec_at(elapsed),
                        );
                    }
                }
                GetProgressItem::Done(value) => {
                    stats = value;

                    let elapsed = transfer_start_time.elapsed().as_secs_f64();
                    download_duration_ms = duration_ms(elapsed);
                    speed.record(elapsed, payload_size);
                    emit_progress_event(
                        app_handle,
                        payload_size,
                        payload_size,
                        speed.bytes_per_sec_at(elapsed),
                    );

                    break;
                }
                GetProgressItem::Error(cause) => {
                    tracing::error!("Download error: {:?}", cause);
                    anyhow::bail!(show_get_error(cause));
                }
            }
        }
        (stats, total_files, payload_size, download_duration_ms)
    } else {
        let total_files = local.children().unwrap() - 1;
        let payload_bytes = 0;

        emit_event(app_handle, "receive-started");
        emit_event(app_handle, "receive-completed");

        (Stats::default(), total_files, payload_bytes, 0)
    };

    let collection = Collection::load(hash_and_format.hash, db).await?;

    let mut file_names: Vec<String> = Vec::new();
    for (name, _hash) in collection.iter() {
        file_names.push(name.to_string());
    }

    if !file_names.is_empty() {
        let file_names_json =
            serde_json::to_string(&file_names).unwrap_or_else(|_| "[]".to_string());
        emit_event_with_payload(app_handle, "receive-file-names", &file_names_json);
    }

    Ok(DownloadToStoreResult {
        collection,
        total_files,
        payload_size,
        stats,
        download_duration_ms,
    })
}

fn show_get_error(e: GetError) -> GetError {
    match &e {
        GetError::InitialNext { source, .. } => {
            tracing::error!("initial connection error: {source}");
        }
        GetError::ConnectedNext { source, .. } => {
            tracing::error!("connected error: {source}");
        }
        GetError::AtBlobHeaderNext { source, .. } => {
            tracing::error!("reading blob header error: {source}");
        }
        GetError::Decode { source, .. } => {
            tracing::error!("decoding error: {source}");
        }
        GetError::IrpcSend { source, .. } => {
            tracing::error!("error sending over irpc: {source}");
        }
        GetError::AtClosingNext { source, .. } => {
            tracing::error!("error at closing: {source}");
        }
        GetError::BadRequest { .. } => {
            tracing::error!("bad request");
        }
        GetError::LocalFailure { source, .. } => {
            tracing::error!("local failure {source:?}");
        }
    }
    e
}
