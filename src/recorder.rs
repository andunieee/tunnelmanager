//! Transfer-history recording driven by the same engine events the UI sees.
//! A slim re-implementation of the Tauri shell's `HistoryRecordingEmitter`.

use crate::engine::{
    unix_now_ms, TransferDirection, TransferHistoryStore, TransferPathType, TransferPeer,
    TransferRecord, TransferStatus,
};
use std::sync::{Arc, Mutex};

#[derive(Debug, Default, Clone)]
pub struct Ctx {
    pub root_name: String,
    pub payload_bytes: u64,
    pub item_count: u32,
    pub path_type: Option<TransferPathType>,
    pub save_path: Option<String>,
    pub peer: Option<TransferPeer>,
    pub blob_hash: Option<String>,
    /// Receive only: the partial store a cancelled/failed download leaves
    /// behind for resume. Recorded at open so deleting the row (or a crash
    /// sweep) can reclaim it; cleared when the transfer completes.
    pub resumable_store_path: Option<String>,
    /// Send only: a pasted text (receives learn it at the end).
    pub text: Option<String>,
}

#[derive(Default)]
struct Row {
    id: Option<String>,
    finalized: bool,
    bytes_transferred: u64,
    file_names: Vec<String>,
    peer_count: u32,
    conflict_count: u32,
}

pub struct Recorder {
    store: Arc<TransferHistoryStore>,
    direction: TransferDirection,
    /// `false` when history recording is turned off in settings: no row is
    /// ever opened, so every later `note`/`finalize` is a no-op.
    enabled: bool,
    ctx: Mutex<Ctx>,
    row: Mutex<Row>,
}

impl Recorder {
    pub fn new(
        store: Arc<TransferHistoryStore>,
        direction: TransferDirection,
        ctx: Ctx,
        enabled: bool,
    ) -> Self {
        Self {
            store,
            direction,
            enabled,
            ctx: Mutex::new(ctx),
            row: Mutex::new(Row::default()),
        }
    }

    pub fn note(&self, event: &str, payload: Option<&str>) {
        match event {
            "transfer-started" | "receive-started" => self.open_row(),
            "transfer-progress" | "receive-progress" => {
                if let Some(bytes) = payload
                    .and_then(|p| p.split(':').next())
                    .and_then(|b| b.parse::<u64>().ok())
                {
                    let mut row = self.row.lock().unwrap_or_else(|p| p.into_inner());
                    row.bytes_transferred = bytes;
                }
            }
            "share-peer-connected" => {
                let mut row = self.row.lock().unwrap_or_else(|p| p.into_inner());
                row.peer_count = row.peer_count.saturating_add(1);
            }
            "receive-file-names" => {
                if let Some(names) = payload.and_then(|p| serde_json::from_str(p).ok()) {
                    let mut row = self.row.lock().unwrap_or_else(|p| p.into_inner());
                    row.file_names = names;
                }
            }
            "receive-conflicts" => {
                let count = payload
                    .and_then(|p| serde_json::from_str::<Vec<serde_json::Value>>(p).ok())
                    .map_or(0, |list| list.len() as u32);
                let mut row = self.row.lock().unwrap_or_else(|p| p.into_inner());
                row.conflict_count = count;
            }
            "transfer-completed" | "receive-completed" => {
                let facts = payload.map(facts_from_payload).unwrap_or_default();
                self.finalize(TransferStatus::Completed, facts.0, facts.1, facts.2, None);
            }
            "transfer-failed" => {
                self.finalize(TransferStatus::Failed, None, None, None, None);
            }
            _ => {}
        }
    }

    /// Closes the row from outside the engine (stop sharing / cancel / error).
    pub fn finalize(
        &self,
        status: TransferStatus,
        duration_ms: Option<u64>,
        export_ms: Option<u64>,
        bytes: Option<u64>,
        error: Option<String>,
    ) {
        let (id, tracked_bytes, file_names, peer_count, conflict_count) = {
            let mut row = self.row.lock().unwrap_or_else(|p| p.into_inner());
            let Some(id) = row.id.clone() else {
                return;
            };
            if row.finalized {
                return;
            }
            row.finalized = true;
            (
                id,
                row.bytes_transferred,
                row.file_names.clone(),
                row.peer_count,
                row.conflict_count,
            )
        };

        let is_receive = self.direction == TransferDirection::Receive;
        let completed = matches!(status, TransferStatus::Completed);
        let shape = received_shape(&file_names);

        let result = self.store.update(&id, |record| {
            record.status = status;
            record.ended_at = Some(unix_now_ms());
            record.duration_ms = duration_ms;
            record.export_ms = export_ms;
            match bytes {
                Some(bytes) => {
                    record.payload_bytes = bytes;
                    record.bytes_transferred = bytes;
                }
                None => record.bytes_transferred = tracked_bytes,
            }
            record.avg_speed_bps = match duration_ms {
                Some(ms) if ms > 0 => Some(record.payload_bytes as f64 / (ms as f64 / 1000.0)),
                _ => None,
            };
            if !file_names.is_empty() {
                record.set_file_names(file_names);
            }
            if is_receive && shape.1 > 0 {
                record.root_name = shape.0;
                record.item_count = shape.1;
                record.path_type = shape.2;
            }
            record.peer_count = peer_count.max(u32::from(record.peer.is_some()));
            if record.peer_count > 1 {
                record.peer = None;
            }
            if conflict_count > 0 {
                record.conflict_count = conflict_count;
            }
            if completed {
                record.resumable_store_path = None;
            }
            record.error = error;
        });

        if let Err(e) = result {
            tracing::warn!("failed to finalize history row: {e}");
        }
    }

    /// Attaches the pasted text once a receive turns out to be one.
    pub fn set_text(&self, text: String) {
        let Some(id) = self.row.lock().unwrap_or_else(|p| p.into_inner()).id.clone() else {
            return;
        };
        let stored = self.store.update(&id, |record| {
            record.text_preview = Some(crate::transfers::text_preview(&text));
            record.text = Some(text);
        });
        if let Err(e) = stored {
            tracing::warn!("failed to store pasted text: {e}");
        }
    }

    fn open_row(&self) {
        if !self.enabled {
            return;
        }
        let mut row = self.row.lock().unwrap_or_else(|p| p.into_inner());
        if row.id.is_some() {
            return;
        }
        let ctx = self.ctx.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let mut record =
            TransferRecord::new(self.direction, ctx.root_name.clone(), ctx.payload_bytes);
        record.item_count = ctx.item_count;
        record.path_type = ctx.path_type;
        record.save_path = ctx.save_path.clone();
        record.peer = ctx.peer.clone();
        record.blob_hash = ctx.blob_hash.clone();
        record.resumable_store_path = ctx.resumable_store_path.clone();
        record.text_preview = ctx.text.as_deref().map(crate::transfers::text_preview);
        record.text = ctx.text.clone();

        match self.store.open(record) {
            Ok(id) => row.id = Some(id),
            Err(e) => tracing::warn!("failed to open history row: {e}"),
        }
    }
}

/// (durationMs, exportMs, bytes) out of a completion payload.
fn facts_from_payload(payload: &str) -> (Option<u64>, Option<u64>, Option<u64>) {
    match serde_json::from_str::<serde_json::Value>(payload) {
        Ok(value) => (
            value.get("durationMs").and_then(|v| v.as_u64()),
            value.get("exportMs").and_then(|v| v.as_u64()),
            value.get("bytes").and_then(|v| v.as_u64()),
        ),
        Err(_) => (None, None, None),
    }
}

/// (root_name, item_count, path_type) a receive learns from its file list.
fn received_shape(file_names: &[String]) -> (String, u32, Option<TransferPathType>) {
    let mut top_level: Vec<&str> = Vec::new();
    for name in file_names {
        let head = name.split('/').next().unwrap_or(name);
        if !top_level.contains(&head) {
            top_level.push(head);
        }
    }
    match top_level.as_slice() {
        [] => (String::new(), 0, None),
        [only] => {
            let is_dir = file_names.iter().any(|n| n.contains('/'));
            (
                (*only).to_string(),
                1,
                Some(if is_dir {
                    TransferPathType::Directory
                } else {
                    TransferPathType::File
                }),
            )
        }
        many => (String::new(), many.len() as u32, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (Arc<TransferHistoryStore>, std::path::PathBuf) {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("tm-slint-recorder-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        (Arc::new(TransferHistoryStore::new(&dir)), dir)
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn received_shape_single_file() {
        let (root, count, kind) = received_shape(&names(&["a.txt"]));
        assert_eq!((root.as_str(), count), ("a.txt", 1));
        assert!(matches!(kind, Some(TransferPathType::File)));
    }

    #[test]
    fn received_shape_single_directory() {
        let (root, count, kind) = received_shape(&names(&["dir/a", "dir/sub/b"]));
        assert_eq!((root.as_str(), count), ("dir", 1));
        assert!(matches!(kind, Some(TransferPathType::Directory)));
    }

    #[test]
    fn received_shape_many_and_empty() {
        let (root, count, kind) = received_shape(&names(&["a", "b", "dir/c"]));
        assert_eq!((root.as_str(), count), ("", 3));
        assert!(kind.is_none());
        assert_eq!(received_shape(&[]).1, 0);
    }

    #[test]
    fn completion_payload_facts() {
        assert_eq!(
            facts_from_payload(r#"{"durationMs":10,"exportMs":2,"bytes":99}"#),
            (Some(10), Some(2), Some(99))
        );
        assert_eq!(facts_from_payload("not json"), (None, None, None));
    }

    #[test]
    fn receive_lifecycle_is_recorded() {
        let (store, dir) = store();
        let recorder = Recorder::new(
            store.clone(),
            TransferDirection::Receive,
            Ctx {
                payload_bytes: 100,
                resumable_store_path: Some("/tmp/partial".into()),
                ..Ctx::default()
            },
            true,
        );
        recorder.note("receive-started", None);
        recorder.note(
            "receive-file-names",
            Some(r#"["photos/a.jpg","photos/b.jpg"]"#),
        );
        recorder.note("receive-progress", Some("50:100:0"));
        recorder.note("receive-conflicts", Some(r#"[{},{}]"#));
        recorder.note(
            "receive-completed",
            Some(r#"{"durationMs":1000,"bytes":100}"#),
        );

        let rows = store.list().unwrap();
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert!(matches!(r.status, TransferStatus::Completed));
        assert_eq!(r.root_name, "photos");
        assert_eq!(r.conflict_count, 2);
        assert_eq!(r.avg_speed_bps, Some(100.0));
        assert!(
            r.resumable_store_path.is_none(),
            "completed rows drop the partial store"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cancelled_receive_keeps_partial_store() {
        let (store, dir) = store();
        let recorder = Recorder::new(
            store.clone(),
            TransferDirection::Receive,
            Ctx {
                resumable_store_path: Some("/tmp/partial".into()),
                ..Ctx::default()
            },
            true,
        );
        recorder.note("receive-started", None);
        recorder.finalize(TransferStatus::Cancelled, None, None, None, None);
        // A second finalize (e.g. a late failure) must not overwrite the first.
        recorder.finalize(TransferStatus::Failed, None, None, None, Some("x".into()));

        let r = &store.list().unwrap()[0];
        assert!(matches!(r.status, TransferStatus::Cancelled));
        assert_eq!(r.resumable_store_path.as_deref(), Some("/tmp/partial"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn disabled_recorder_writes_nothing() {
        let (store, dir) = store();
        let recorder = Recorder::new(
            store.clone(),
            TransferDirection::Send,
            Ctx::default(),
            false,
        );
        recorder.note("transfer-started", None);
        recorder.note("transfer-completed", Some("{}"));
        assert!(store.list().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
