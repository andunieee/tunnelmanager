use crate::protocol::{download_to_store, AppHandle};
use iroh::Endpoint;
use iroh_blobs::ticket::BlobTicket;
use std::path::PathBuf;
use tokio::select;
use crate::native::export::export_to_directory;
use crate::native::storage;
use crate::native::types::ReceiveResult;

fn emit_event_with_payload(app_handle: &AppHandle, event_name: &str, payload: &str) {
    if let Some(handle) = app_handle {
        if let Err(e) = handle.emit_event_with_payload(event_name, payload) {
            tracing::warn!("Failed to emit event {} with payload: {}", event_name, e);
        }
    }
}

/// Downloads the share `ticket` names over `endpoint` and writes it to
/// `output_dir`. A partial download stays on disk to resume from.
pub(crate) async fn download(
    endpoint: &Endpoint,
    ticket: BlobTicket,
    output_dir: PathBuf,
    app_handle: AppHandle,
    cancel_rx: tokio::sync::oneshot::Receiver<()>,
) -> anyhow::Result<ReceiveResult> {
    let addr = ticket.addr().clone();
    let (db, iroh_data_dir) =
        storage::create_recv_store(&ticket.hash().to_hex().to_string()).await?;
    let mut cleanup_guard = storage::recv_cleanup_guard(iroh_data_dir);
    let db2 = db.clone();

    let transfer = async {
        let downloaded =
            download_to_store(ticket, addr, endpoint, db.as_ref(), &app_handle).await?;

        let export_start = std::time::Instant::now();
        let conflicts = export_to_directory(&db, downloaded.collection, &output_dir).await?;
        let export_duration_ms =
            crate::protocol::duration_ms(export_start.elapsed().as_secs_f64());

        if !conflicts.is_empty() {
            let payload = serde_json::to_string(&conflicts).unwrap_or_else(|_| "[]".to_string());
            emit_event_with_payload(&app_handle, "receive-conflicts", &payload);
        }

        // Writing the files out to disk is a separate cost from the transfer;
        // report them apart so both ends can compare like with like.
        // `outputDir` is where the bytes actually landed. The UI cannot re-derive
        // it once an auto-accepted transfer files itself under a per-device
        // subfolder, so "Open" would otherwise reveal the wrong directory.
        let completion = serde_json::json!({
            "durationMs": downloaded.download_duration_ms,
            "exportMs": export_duration_ms,
            "bytes": downloaded.payload_size,
            "outputDir": output_dir.to_string_lossy(),
        });
        emit_event_with_payload(&app_handle, "receive-completed", &completion.to_string());

        anyhow::Ok((
            downloaded.total_files,
            downloaded.payload_size,
            downloaded.stats,
            conflicts.len(),
        ))
    };

    let (total_files, payload_size, _stats, conflict_count) = match select! {
        result = transfer => result,
        _ = cancel_rx => {
            tracing::info!("Download cancelled by user, preserving partial store for resume");
            cleanup_guard.disarm();
            db2.shutdown().await?;
            anyhow::bail!("cancelled");
        }
    } {
        Ok(values) => {
            // Close the store and delete it before returning, not in the
            // background: receiving the same content again reopens this
            // directory. A still-locked database deadlocks iroh-blobs, and a
            // half-deleted one would be mistaken for (or break) a resume.
            db2.shutdown().await?;
            cleanup_guard.disarm();
            let dir = cleanup_guard.path().to_path_buf();
            match tokio::task::spawn_blocking(move || std::fs::remove_dir_all(&dir)).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::warn!("Failed to remove receive store: {e}"),
                Err(e) => tracing::warn!("Receive store cleanup task failed: {e}"),
            }
            values
        }
        Err(e) => {
            tracing::error!("Download operation failed: {e}");
            cleanup_guard.disarm();
            db2.shutdown().await?;
            anyhow::bail!("error: {e}");
        }
    };

    let message = if conflict_count > 0 {
        format!(
            "Downloaded {} files, {} bytes ({} name conflicts auto-resolved)",
            total_files, payload_size, conflict_count
        )
    } else {
        format!("Downloaded {} files, {} bytes", total_files, payload_size)
    };

    Ok(ReceiveResult {
        message,
        file_path: output_dir,
    })
}
