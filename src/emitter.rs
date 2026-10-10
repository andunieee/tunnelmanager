use std::sync::{Arc, Mutex};

use crate::format::{fmt_bytes, fmt_speed, parse_progress};
use crate::TransferRow;

/// One node/transfer event: (name, json payload).
pub type MainEvent = (String, Option<String>);

/// Queue of events from the long-lived `NodeService`. Drained by the UI.
#[derive(Clone, Default)]
pub struct MainQueue {
    events: Arc<Mutex<Vec<MainEvent>>>,
}

impl MainQueue {
    pub fn drain(&self) -> Vec<MainEvent> {
        std::mem::take(&mut *self.events.lock().unwrap())
    }

    fn push(&self, name: &str, payload: Option<String>) {
        self.events
            .lock()
            .unwrap()
            .push((name.to_string(), payload));
    }
}

/// `crate::engine::EventEmitter` bound to the node service.
pub struct MainEmitter {
    queue: MainQueue,
}

impl MainEmitter {
    pub fn new(queue: MainQueue) -> Self {
        Self { queue }
    }
}

impl crate::engine::EventEmitter for MainEmitter {
    fn emit_event(&self, event_name: &str) -> Result<(), String> {
        self.queue.push(event_name, None);
        Ok(())
    }

    fn emit_event_with_payload(&self, event_name: &str, payload: &str) -> Result<(), String> {
        self.queue.push(event_name, Some(payload.to_string()));
        Ok(())
    }
}

/// Fold one engine transfer event into the UI row of that transfer.
pub fn apply_transfer_event(row: &mut TransferRow, name: &str, payload: Option<&str>) {
    match name {
        "transfer-started" | "receive-started" => {
            row.status = "Transferring…".into();
            row.error = "".into();
        }
        "share-peer-connected" => {
            row.status = "Peer connected, transferring…".into();
        }
        "transfer-progress" | "receive-progress" => {
            if let Some((bytes, total, speed)) = payload.and_then(parse_progress) {
                if total > 0 {
                    let frac = (bytes as f32 / total as f32).min(1.0);
                    row.progress = frac;
                    row.progress_label = format!(
                        "{}% · {} of {}",
                        (frac * 100.0) as u32,
                        fmt_bytes(bytes),
                        fmt_bytes(total)
                    )
                    .into();
                }
                row.speed = fmt_speed(speed).into();
            }
        }
        "transfer-completed" => {
            row.progress = 1.0;
            row.progress_label = "100%".into();
            row.speed = "".into();
            row.status = "Sent".into();
            row.error = "".into();
        }
        "receive-completed" => {
            row.progress = 1.0;
            row.progress_label = "100%".into();
            row.speed = "".into();
            let out = json_str(payload, "outputDir").unwrap_or_default();
            row.status = if out.is_empty() {
                "Received".into()
            } else {
                format!("Saved to {out}").into()
            };
        }
        "transfer-failed" => {
            // The share stays open: the receiver may retry and resume.
            row.speed = "".into();
            row.error = "Transfer interrupted. The peer can retry while this stays open.".into();
        }
        "receive-conflicts" => {
            let count = payload
                .and_then(|p| serde_json::from_str::<serde_json::Value>(p).ok())
                .and_then(|v| v.as_array().map(Vec::len))
                .unwrap_or(0);
            if count > 0 {
                let files = if count == 1 { "file" } else { "files" };
                row.note = format!("{count} {files} renamed to avoid overwriting").into();
            }
        }
        _ => {}
    }
}

fn json_str(payload: Option<&str>, key: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(payload?).ok()?;
    value.get(key)?.as_str().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row() -> TransferRow {
        TransferRow {
            active: true,
            ..Default::default()
        }
    }

    #[test]
    fn progress_sets_fraction_label_and_speed() {
        let mut r = row();
        apply_transfer_event(&mut r, "receive-progress", Some("512:2048:1024000"));
        assert_eq!(r.progress, 0.25);
        assert_eq!(r.progress_label.as_str(), "25% · 512 B of 2.0 KB");
        assert_eq!(r.speed.as_str(), "1.0 KB/s");
    }

    #[test]
    fn progress_with_unknown_total_keeps_bar() {
        let mut r = row();
        r.progress = 0.5;
        apply_transfer_event(&mut r, "transfer-progress", Some("10:0:0"));
        assert_eq!(r.progress, 0.5);
        assert_eq!(r.speed.as_str(), "-");
    }

    #[test]
    fn malformed_progress_is_ignored() {
        let mut r = row();
        apply_transfer_event(&mut r, "transfer-progress", Some("garbage"));
        assert_eq!(r.progress, 0.0);
        assert!(r.progress_label.is_empty());
    }

    #[test]
    fn receive_completed_reports_output_dir() {
        let mut r = row();
        apply_transfer_event(
            &mut r,
            "receive-completed",
            Some(r#"{"outputDir":"/tmp/x","bytes":3}"#),
        );
        assert_eq!(r.progress, 1.0);
        assert_eq!(r.status.as_str(), "Saved to /tmp/x");
    }

    #[test]
    fn conflicts_survive_completion() {
        let mut r = row();
        apply_transfer_event(&mut r, "receive-conflicts", Some(r#"[{"a":1},{"b":2}]"#));
        apply_transfer_event(&mut r, "receive-completed", Some(r#"{"outputDir":"/d"}"#));
        assert_eq!(r.note.as_str(), "2 files renamed to avoid overwriting");
        assert_eq!(r.status.as_str(), "Saved to /d");
    }

    #[test]
    fn failure_then_restart_clears_error() {
        let mut r = row();
        apply_transfer_event(&mut r, "transfer-failed", None);
        assert!(!r.error.is_empty());
        apply_transfer_event(&mut r, "transfer-started", None);
        assert!(r.error.is_empty());
    }
}
