use crate::emitter::{MainEmitter, MainQueue};
use crate::format;
use crate::platform::{self, copy_to_clipboard, open_path, pick_downloads_folder, toast};
use crate::settings::Settings;
use crate::transfers::{self, Transfers};
use crate::{AppWindow, HistoryRow, Logic, PeerRow, State};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel, Weak};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use crate::engine::{
    get_relay_status, is_reclaimable_partial, reclaim_partial, resolve_relay_mode_with_fallback,
    verify_discovery, verify_relays, AppHandle, NodeService, PairedDeviceInfo, TransferDirection,
    TransferHistoryStore, TransferRecord, TransferStatus,
};

/// All state shared across threads. Cloned freely into async tasks.
#[derive(Clone)]
pub(crate) struct AppCtx {
    pub weak: Weak<AppWindow>,
    pub rt: tokio::runtime::Handle,
    pub history: Arc<TransferHistoryStore>,
    pub settings: Arc<Mutex<Settings>>,
    pub settings_path: PathBuf,
    node: Arc<Mutex<Option<Arc<NodeService>>>>,
    pair_requests: Arc<Mutex<Vec<(String, String)>>>,
    queue: MainQueue,
    pub transfers: Transfers,
}

impl AppCtx {
    /// The node service, once started.
    pub fn node(&self) -> Option<Arc<NodeService>> {
        self.node.lock().unwrap().clone()
    }
}

/// Up to two letters for an avatar: "Living Room" → "LR", "laptop" → "L".
fn initials(name: &str) -> String {
    name.split_whitespace()
        .filter_map(|word| word.chars().find(|c| c.is_alphanumeric()))
        .take(2)
        .flat_map(char::to_uppercase)
        .collect()
}

pub(crate) fn short_id(endpoint_id: &str) -> String {
    endpoint_id.chars().take(8).collect()
}

/// (label, tone) for a history status; tone picks the badge colour.
fn status_badge(status: TransferStatus) -> (&'static str, &'static str) {
    match status {
        TransferStatus::InProgress => ("In progress", "active"),
        TransferStatus::Completed => ("Completed", "ok"),
        TransferStatus::Failed => ("Failed", "error"),
        TransferStatus::Cancelled => ("Cancelled", "muted"),
        TransferStatus::Interrupted => ("Interrupted", "warn"),
    }
}

fn row_from_record(record: &TransferRecord) -> HistoryRow {
    let title = if !record.root_name.is_empty() {
        record.root_name.clone()
    } else {
        record
            .file_names
            .first()
            .cloned()
            .unwrap_or_else(|| "Transfer".to_string())
    };
    let title = if title == transfers::PASTE_FILE_NAME {
        "Pasted text".to_string()
    } else {
        title
    };
    let mut detail = String::new();
    if record.item_count > 1 {
        detail.push_str(&format!("{} items", record.item_count));
    } else if let Some(name) = record.file_names.first() {
        detail.push_str(name);
    }
    if let Some(path) = record.save_path.as_deref() {
        if !detail.is_empty() {
            detail.push_str(" · ");
        }
        detail.push_str(path);
    }
    if record.conflict_count > 0 {
        detail.push_str(&format!(" · {} renamed", record.conflict_count));
    }
    let (status, tone) = status_badge(record.status);

    HistoryRow {
        id: record.id.clone().into(),
        title: title.into(),
        direction: match record.direction {
            TransferDirection::Send => "send",
            TransferDirection::Receive => "receive",
        }
        .into(),
        status: status.into(),
        tone: tone.into(),
        detail: detail.into(),
        date: format::fmt_date(record.started_at).into(),
        size: format::fmt_bytes(record.payload_bytes).into(),
        speed: record
            .avg_speed_bps
            .map(format::fmt_speed)
            .unwrap_or_else(|| "-".to_string())
            .into(),
        can_open: record.save_path.is_some() || record.text.is_some(),
        preview: record.text_preview.clone().unwrap_or_default().into(),
    }
}

fn paired_row(d: &PairedDeviceInfo) -> PeerRow {
    let name = if d.display_name.trim().is_empty() {
        short_id(&d.endpoint_id)
    } else {
        d.display_name.clone()
    };
    let detail = [d.device_type.trim(), d.os.trim(), &short_id(&d.endpoint_id)]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" · ");
    PeerRow {
        // Lowercased like every other id the UI compares against.
        endpoint_id: d.endpoint_id.to_lowercase().into(),
        initials: initials(&name).into(),
        name: name.into(),
        detail: detail.into(),
        online: d.online,
        is_request: false,
        is_suggestion: false,
        trusted: d.trusted,
        ..Default::default()
    }
}

fn request_row(id: &str, name: &str) -> PeerRow {
    PeerRow {
        endpoint_id: id.to_string().into(),
        initials: initials(name).into(),
        name: name.to_string().into(),
        detail: "wants to pair with you".into(),
        online: true,
        is_request: true,
        ..Default::default()
    }
}

fn nearby_row(n: &crate::engine::NearbyDevice) -> PeerRow {
    let id = n.endpoint_id.to_lowercase();
    let name = match &n.display_name {
        Some(name) if !name.trim().is_empty() => name.clone(),
        _ if !n.fingerprint.trim().is_empty() => n.fingerprint.clone(),
        _ => short_id(&n.endpoint_id),
    };
    PeerRow {
        endpoint_id: id.into(),
        initials: initials(&name).into(),
        name: name.into(),
        detail: if n.identified {
            "Found on your local network".into()
        } else {
            "On your local network (unidentified)".into()
        },
        online: true,
        is_suggestion: true,
        ..Default::default()
    }
}

// -------------------------------------------------------- ui refreshers

/// Mirror a peer row into the selected-peer header fields (or clear them).
fn show_selected(state: &State<'_>, row: Option<&PeerRow>) {
    match row {
        Some(row) => {
            state.set_selected_id(row.endpoint_id.clone());
            state.set_selected_name(row.name.clone());
            state.set_selected_detail(row.detail.clone());
            state.set_selected_online(row.online);
            state.set_selected_initials(row.initials.clone());
        }
        None => {
            state.set_selected_id("".into());
            state.set_selected_name("".into());
            state.set_selected_detail("".into());
            state.set_selected_online(false);
            state.set_selected_initials("".into());
        }
    }
}

pub(crate) fn refresh_peers(ctx: &AppCtx) {
    let Some(node) = ctx.node() else {
        return;
    };
    // The paired-device store is read from disk: keep that off the UI thread.
    let ctx = ctx.clone();
    ctx.rt.clone().spawn_blocking(move || {
        let devices = node.list_paired().unwrap_or_default();
        let my_name = node.device_info().display_name;
        let ready = node.is_network_ready();
        let ticket = node.pairing_ticket();
        let ctx_ui = ctx.clone();
        let _ = ctx.weak.upgrade_in_event_loop(move |ui| {
            let state = ui.global::<State>();
            let rows: Vec<PeerRow> = devices.iter().map(paired_row).collect();
            let online_count = devices.iter().filter(|d| d.online).count();
            // Keep the selection (re-reading its name/presence, which may
            // have changed); fall back to the first peer when nothing is
            // selected or the selected peer is gone.
            let selected = state.get_selected_id();
            let current = rows
                .iter()
                .find(|r| r.endpoint_id == selected)
                .or(rows.first());
            let selection_changed = current.is_none_or(|r| r.endpoint_id != selected);
            show_selected(&state, current);
            state.set_peers(ModelRc::from(Rc::new(VecModel::from(rows))));
            transfers::sync_peer_activity(&ui);
            state.set_presence_label(format!("{online_count} of {} online", devices.len()).into());
            state.set_my_name(my_name.into());
            state.set_node_ready(ready);
            match ticket {
                Ok(ticket) => state.set_my_ticket(ticket.into()),
                Err(e) => tracing::debug!("pairing_ticket unavailable: {e}"),
            }
            if selection_changed {
                state.set_rename_mode(false);
                refresh_history(&ctx_ui);
            }
        });
    });
}

fn refresh_suggestions(ctx: &AppCtx) {
    let Some(node) = ctx.node() else {
        return;
    };
    let weak = ctx.weak.clone();
    let requests = ctx.pair_requests.lock().unwrap().clone();
    ctx.rt.spawn(async move {
        let nearby = node.list_nearby().await;
        let reason = node.nearby_unavailable_reason();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let state = ui.global::<State>();
            let mut rows: Vec<PeerRow> = Vec::new();
            for (id, name) in &requests {
                rows.push(request_row(id, name));
            }
            for d in &nearby {
                let id = d.endpoint_id.to_lowercase();
                if requests.iter().any(|(i, _)| i == &id) {
                    continue;
                }
                rows.push(nearby_row(d));
            }
            state.set_suggestions(ModelRc::from(Rc::new(VecModel::from(rows))));
            state.set_nearby_note(reason.unwrap_or_default().into());
        });
    });
}

pub(crate) fn refresh_history(ctx: &AppCtx) {
    let selected = ctx
        .weak
        .upgrade()
        .map(|ui| {
            ui.global::<State>()
                .get_selected_id()
                .to_string()
                .to_lowercase()
        })
        .unwrap_or_default();
    let history = ctx.history.clone();
    let weak = ctx.weak.clone();
    ctx.rt.spawn_blocking(move || {
        let rows: Vec<HistoryRow> = history
            .list()
            .map(|records| {
                records
                    .iter()
                    .filter(|r| {
                        !selected.is_empty()
                            && r.peer
                                .as_ref()
                                .map(|p| p.endpoint_id.to_lowercase() == selected)
                                .unwrap_or(false)
                    })
                    // The store lists newest first already.
                    .map(row_from_record)
                    .collect()
            })
            .unwrap_or_default();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                ui.global::<State>()
                    .set_history(ModelRc::from(Rc::new(VecModel::from(rows))));
            }
        });
    });
}

/// Relay URLs as typed: one per line, or separated by commas/spaces.
fn split_urls(text: &str) -> Vec<String> {
    text.split(|c: char| c.is_whitespace() || c == ',')
        .filter(|url| !url.is_empty())
        .map(str::to_string)
        .collect()
}

/// Read the settings-page fields back into a `Settings` value.
fn settings_from_state(state: &State<'_>) -> Settings {
    let non_empty = |value: SharedString| (!value.trim().is_empty()).then(|| value.to_string());
    Settings {
        downloads_dir: non_empty(state.get_downloads_dir()),
        relay_mode: match state.get_relay_mode() {
            1 => "disabled",
            2 => "custom",
            _ => "default",
        }
        .to_string(),
        relay_urls: split_urls(&state.get_relay_urls()),
        relay_token: non_empty(state.get_relay_token()),
        relay_fallback: match state.get_relay_fallback() {
            1 => "public",
            _ => "strict",
        }
        .to_string(),
        discovery_mode: match state.get_discovery_mode() {
            1 => "custom",
            _ => "default",
        }
        .to_string(),
        discovery_pkarr_relay_url: non_empty(state.get_discovery_pkarr_url()),
        discovery_dns_origin: non_empty(state.get_discovery_dns_origin()),
        history_enabled: state.get_history_enabled(),
        discoverability: match state.get_discoverability() {
            1 => "paired-only",
            2 => "off",
            _ => "everyone",
        }
        .to_string(),
    }
}

/// Fill the settings-page fields from `settings` (inverse of `settings_from_state`).
fn settings_into_state(state: &State<'_>, s: &Settings) {
    let downloads_dir = s
        .downloads_base()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    state.set_downloads_dir(downloads_dir.into());
    state.set_relay_mode(match s.relay_mode.as_str() {
        "disabled" => 1,
        "custom" => 2,
        _ => 0,
    });
    state.set_relay_urls(s.relay_urls.join("\n").into());
    state.set_relay_token(s.relay_token.clone().unwrap_or_default().into());
    state.set_relay_fallback(match s.relay_fallback.as_str() {
        "public" => 1,
        _ => 0,
    });
    state.set_discovery_mode(match s.discovery_mode.as_str() {
        "custom" => 1,
        _ => 0,
    });
    state.set_discovery_pkarr_url(
        s.discovery_pkarr_relay_url
            .clone()
            .unwrap_or_default()
            .into(),
    );
    state.set_discovery_dns_origin(s.discovery_dns_origin.clone().unwrap_or_default().into());
    state.set_discoverability(match s.discoverability.as_str() {
        "paired-only" => 1,
        "off" => 2,
        _ => 0,
    });
    state.set_history_enabled(s.history_enabled);
}

// --------------------------------------------------- node event routing

fn handle_main_event(ctx: &AppCtx, name: &str, payload: Option<&str>) {
    let parsed = payload.and_then(|p| serde_json::from_str::<serde_json::Value>(p).ok());

    match name {
        "device-node-network-ready" => {
            if let Some(ui) = ctx.weak.upgrade() {
                ui.global::<State>().set_node_ready(true);
                toast(&ui, "Network ready", false);
            }
        }
        "device-node-network-warming" => {
            if let Some(ui) = ctx.weak.upgrade() {
                ui.global::<State>().set_node_ready(false);
                toast(&ui, "Connecting to network…", false);
            }
        }
        "relay-fell-back" => {
            if let Some(ui) = ctx.weak.upgrade() {
                let reason = parsed
                    .as_ref()
                    .and_then(|v| v.get("reason"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("unreachable relay");
                toast(&ui, &format!("Relay fell back to public: {reason}"), true);
            }
        }
        "device-paired" => {
            let who = parsed
                .as_ref()
                .and_then(|v| v.get("display_name"))
                .and_then(|v| v.as_str())
                .unwrap_or("a peer");
            if let Some(ui) = ctx.weak.upgrade() {
                toast(&ui, &format!("Paired with {who}"), false);
            }
            refresh_peers(ctx);
            refresh_suggestions(ctx);
        }
        "device-unpaired" => {
            refresh_peers(ctx);
            refresh_suggestions(ctx);
        }
        "paired-device-presence" => refresh_peers(ctx),
        "nearby-device-found" | "nearby-device-identified" | "nearby-device-lost" => {
            refresh_suggestions(ctx)
        }
        "nearby-unavailable" => {
            let reason = parsed
                .as_ref()
                .and_then(|v| v.get("reason"))
                .and_then(|v| v.as_str())
                .unwrap_or("unavailable")
                .to_string();
            if let Some(ui) = ctx.weak.upgrade() {
                ui.global::<State>().set_nearby_note(reason.into());
            }
        }
        "nearby-pair-request-received" => {
            let id = parsed
                .as_ref()
                .and_then(|v| v.get("remote_endpoint_id"))
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_lowercase();
            let sender = parsed
                .as_ref()
                .and_then(|v| v.get("sender_name"))
                .and_then(|v| v.as_str())
                .unwrap_or("A device")
                .to_string();
            if !id.is_empty() {
                {
                    let mut reqs = ctx.pair_requests.lock().unwrap();
                    if !reqs.iter().any(|(i, _)| i == &id) {
                        reqs.push((id, sender.clone()));
                    }
                }
                if let Some(ui) = ctx.weak.upgrade() {
                    toast(
                        &ui,
                        &format!("{sender} wants to pair. See “Add a peer”"),
                        false,
                    );
                }
                refresh_suggestions(ctx);
            }
        }
        "paired-invite-response" => {
            let response = parsed
                .as_ref()
                .and_then(|v| v.get("response"))
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            // An accept is announced by the `device-paired` event (and its
            // toast); only a decline needs surfacing here.
            if response == "declined" {
                let who = parsed
                    .as_ref()
                    .and_then(|v| v.get("display_name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("that device");
                if let Some(ui) = ctx.weak.upgrade() {
                    toast(&ui, &format!("{who} declined your pair request"), true);
                }
            }
        }
        "paired-invite-received" => {
            transfers::accept_invite(ctx, parsed.unwrap_or_default());
        }
        _ => {}
    }
}

// ------------------------------------------------------------- node start

fn start_node(ctx: &AppCtx) {
    let data_dir = ctx
        .settings_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let settings = ctx.settings.clone();
    let queue = ctx.queue.clone();
    let ctx_bg = ctx.clone();

    ctx.rt.spawn(async move {
        let cfg = settings.lock().unwrap().clone();
        let discovery_mode = cfg.discovery_mode();
        let discoverability = cfg.discoverability();
        let (relay, fell_back) =
            match resolve_relay_mode_with_fallback(Some(cfg.relay_config_arg())).await {
                Ok(v) => v,
                Err(e) => {
                    // Strict policy: keep the configured relays (they may come
                    // up later) rather than silently switching to public ones.
                    tracing::error!("failed to resolve relay mode at startup: {e}");
                    platform::toast_later(&ctx_bg.weak, format!("Relay unreachable: {e}"), true);
                    (cfg.relay_mode(), false)
                }
            };
        if fell_back {
            tracing::warn!("custom relay unreachable at startup; fell back to public relays");
        }
        let relay_mode: iroh::endpoint::RelayMode = relay.into();
        let emitter: AppHandle = Some(Arc::new(MainEmitter::new(queue)));
        match NodeService::start_with_bluetooth(
            &data_dir,
            relay_mode,
            discovery_mode,
            discoverability,
            emitter,
            crate::android::bluetooth_hub(),
        )
        .await
        {
            Ok(node) => {
                let node = Arc::new(node);
                watch_network(&ctx_bg, &node);
                *ctx_bg.node.lock().unwrap() = Some(node);
                refresh_peers(&ctx_bg);
                refresh_suggestions(&ctx_bg);
                let weak = ctx_bg.weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        toast(&ui, "Connected, peering is live", false);
                    }
                });
            }
            Err(e) => {
                tracing::error!("failed to start node service: {e}");
                let msg = format!("Node start failed: {e}");
                let weak = ctx_bg.weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        toast(&ui, &msg, true);
                    }
                });
            }
        }
    });
}

/// Passes the platform's network changes to `node` (only Android reports
/// any: iroh can't see them there). They come in bursts, one per network
/// and property, so a burst settles into one `network_changed`.
fn watch_network(ctx: &AppCtx, node: &Arc<NodeService>) {
    const SETTLE: std::time::Duration = std::time::Duration::from_millis(500);
    let changed = Arc::new(tokio::sync::Notify::new());
    let node = Arc::downgrade(node);
    let waiter = changed.clone();
    ctx.rt.spawn(async move {
        loop {
            waiter.notified().await;
            tokio::time::sleep(SETTLE).await;
            let Some(node) = node.upgrade() else {
                return;
            };
            tracing::info!("network changed");
            node.network_changed().await;
        }
    });
    // `notify_one` keeps a permit while the loop is busy, so a change during
    // a `network_changed` still gets its own pass.
    crate::android::watch_network(move || changed.notify_one());
}

// -------------------------------------------------------- register_* fns

fn register_node(ctx: &AppCtx) {
    start_node(ctx);
    let timer = slint::Timer::default();
    let queue = ctx.queue.clone();
    let ctx_bg = ctx.clone();
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(150),
        move || {
            // Timer callbacks already run on the UI thread.
            for (name, payload) in queue.drain() {
                handle_main_event(&ctx_bg, &name, payload.as_deref());
            }
        },
    );
    std::mem::forget(timer);
}

fn begin_rename(ctx: &AppCtx) {
    if let Some(ui) = ctx.weak.upgrade() {
        let state = ui.global::<State>();
        state.set_rename_mode(true);
        state.set_rename_input(state.get_selected_name());
    }
}

fn confirm_rename(ctx: &AppCtx) {
    let Some(node) = ctx.node() else {
        return;
    };
    let (id, name) = {
        let Some(ui) = ctx.weak.upgrade() else {
            return;
        };
        let state = ui.global::<State>();
        (
            state.get_selected_id().to_string(),
            state.get_rename_input().to_string().trim().to_string(),
        )
    };
    if id.is_empty() || name.is_empty() {
        return;
    }
    let weak = ctx.weak.clone();
    let ctx_bg = ctx.clone();
    ctx.rt.spawn(async move {
        match node.rename_paired(&id, &name) {
            Ok(_) => {
                let ctx_done = ctx_bg.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        ui.global::<State>().set_rename_mode(false);
                        toast(&ui, "Peer renamed", false);
                        refresh_peers(&ctx_done);
                    }
                });
            }
            Err(e) => {
                let msg = format!("Rename failed: {e}");
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        toast(&ui, &msg, true);
                    }
                });
            }
        }
    });
}

fn register_peers(ctx: &AppCtx) {
    let Some(ui) = ctx.weak.upgrade() else {
        return;
    };
    let logic = ui.global::<Logic>();

    {
        let ctx = ctx.clone();
        logic.on_refresh_peers(move || {
            refresh_peers(&ctx);
            refresh_suggestions(&ctx);
        });
    }
    {
        let ctx = ctx.clone();
        logic.on_select_peer(move |id: SharedString| {
            let Some(ui) = ctx.weak.upgrade() else {
                return;
            };
            let state = ui.global::<State>();
            let id = id.to_lowercase();
            let Some(row) = state.get_peers().iter().find(|r| r.endpoint_id == id) else {
                return;
            };
            state.set_rename_mode(false);
            show_selected(&state, Some(&row));
            transfers::sync_peer_activity(&ui);
            refresh_history(&ctx);
        });
    }
    {
        let ctx = ctx.clone();
        logic.on_rename_peer(move || begin_rename(&ctx));
    }
    {
        let ctx = ctx.clone();
        logic.on_rename_peer_cancel(move || {
            if let Some(ui) = ctx.weak.upgrade() {
                ui.global::<State>().set_rename_mode(false);
            }
        });
    }
    {
        let ctx = ctx.clone();
        logic.on_rename_peer_confirm(move || confirm_rename(&ctx));
    }
    {
        let ctx = ctx.clone();
        logic.on_remove_peer(move || {
            let Some(node) = ctx.node() else {
                return;
            };
            let (id, name) = {
                let Some(ui) = ctx.weak.upgrade() else { return };
                let state = ui.global::<State>();
                let id = state.get_selected_id().to_string();
                let name = state.get_selected_name().to_string();
                drop(state);
                (id, name)
            };
            if id.is_empty() {
                return;
            }
            let weak = ctx.weak.clone();
            let ctx_bg = ctx.clone();
            ctx.rt.spawn(async move {
                if let Err(e) = node.forget_paired(&id).await {
                    tracing::warn!("forget {id} failed: {e}");
                    platform::toast_later(&weak, format!("Could not forget {name}: {e}"), true);
                    return;
                }
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        let state = ui.global::<State>();
                        show_selected(&state, None);
                        state.set_history(ModelRc::from(Rc::new(VecModel::from(
                            Vec::<HistoryRow>::new(),
                        ))));
                        drop(state);
                        toast(&ui, &format!("Forgot {name}"), false);
                        refresh_peers(&ctx_bg);
                    }
                });
            });
        });
    }
}

fn register_add_peer(ctx: &AppCtx) {
    let Some(ui) = ctx.weak.upgrade() else {
        return;
    };
    let logic = ui.global::<Logic>();

    {
        // Paste an address → join_pairing
        let ctx = ctx.clone();
        logic.on_pair_with_pasted(move || {
            let Some(node) = ctx.node() else {
                return;
            };
            let (ticket, weak, ctx_bg) = {
                let Some(ui) = ctx.weak.upgrade() else { return };
                let state = ui.global::<State>();
                let ticket = state.get_add_ticket_input().to_string().trim().to_string();
                drop(ui);

                if ticket.is_empty() {
                    let weak = ctx.weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            let s = ui.global::<State>();
                            s.set_pairing_status("Paste an address first".into());
                            s.set_pairing_error(true);
                        }
                    });
                    return;
                }

                if let Some(ui) = ctx.weak.upgrade() {
                    let s = ui.global::<State>();
                    s.set_pairing_busy(true);
                    s.set_pairing_status("".into());
                    s.set_pairing_error(false);
                }
                (ticket, ctx.weak.clone(), ctx.clone())
            };

            let rt_bg = ctx_bg.rt.clone();
            rt_bg.spawn(async move {
                match node.join_pairing(&ticket).await {
                    Ok(()) => {
                        let weak2 = weak.clone();
                        let ctx_bg2 = ctx_bg.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = weak2.upgrade() {
                                let s = ui.global::<State>();
                                s.set_pairing_busy(false);
                                s.set_pairing_status("Peer added".into());
                                s.set_pairing_error(false);
                                s.set_add_ticket_input("".into());
                            }
                        });
                        let ctx_bg3 = ctx_bg.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            refresh_peers(&ctx_bg2);
                            refresh_suggestions(&ctx_bg3);
                        });
                    }
                    Err(e) => {
                        let msg = format!("Could not pair: {e}");
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = weak.upgrade() {
                                let s = ui.global::<State>();
                                s.set_pairing_busy(false);
                                s.set_pairing_status(msg.clone().into());
                                s.set_pairing_error(true);
                            }
                        });
                    }
                }
            });
        });
    }

    {
        // Pair a suggestion: an inbound pair request is accepted with the
        // already-committed invite; a nearby device gets a pair request the
        // other side must still accept, so it reports "sent", not "added".
        let ctx = ctx.clone();
        logic.on_accept_suggestion(move |id: SharedString| {
            let Some(node) = ctx.node() else {
                return;
            };
            let weak = ctx.weak.clone();
            let ctx_bg = ctx.clone();
            ctx.rt.spawn(async move {
                let id = id.to_string();
                let is_request = ctx_bg
                    .pair_requests
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(i, _)| i == &id.to_lowercase());

                let (success, status) = if is_request {
                    match node.accept_nearby_invite(&id).await {
                        Ok(()) => {
                            ctx_bg
                                .pair_requests
                                .lock()
                                .unwrap()
                                .retain(|(i, _)| i != &id.to_lowercase());
                            (true, "Peer added".to_string())
                        }
                        Err(e) => (false, format!("Could not pair: {e}")),
                    }
                } else {
                    match node.request_nearby_pair(&id).await {
                        Ok(true) => (
                            true,
                            "Pair request sent, waiting for them to accept".to_string(),
                        ),
                        Ok(false) => (
                            false,
                            "Couldn't reach them. Are you on the same network?".to_string(),
                        ),
                        Err(e) => (false, format!("Could not pair: {e}")),
                    }
                };

                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        let s = ui.global::<State>();
                        s.set_pairing_status(status.into());
                        s.set_pairing_error(!success);
                    }
                });
                refresh_peers(&ctx_bg);
                refresh_suggestions(&ctx_bg);
            });
        });
    }

    {
        // decline an inbound pair request
        let ctx = ctx.clone();
        logic.on_decline_suggestion(move |id: SharedString| {
            let Some(node) = ctx.node() else {
                return;
            };
            {
                let mut reqs = ctx.pair_requests.lock().unwrap();
                reqs.retain(|(i, _)| i != &id.to_string().to_lowercase());
            }
            let ctx_bg = ctx.clone();
            ctx.rt.spawn(async move {
                if let Err(e) = node.decline_nearby_invite(&id, false).await {
                    tracing::warn!("decline {id} failed: {e}");
                }
                refresh_suggestions(&ctx_bg);
            });
        });
    }

    {
        // copy my pairing ticket
        let weak = ctx.weak.clone();
        logic.on_copy_my_ticket(move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let ticket = ui.global::<State>().get_my_ticket().to_string();
            if ticket.is_empty() {
                return;
            }
            match copy_to_clipboard(&ticket) {
                Ok(()) => toast(&ui, "Invite copied. Share it with your peer", false),
                Err(e) => toast(&ui, &format!("Copy failed: {e}"), true),
            }
        });
    }
}

fn register_history(ctx: &AppCtx) {
    let Some(ui) = ctx.weak.upgrade() else {
        return;
    };
    let logic = ui.global::<Logic>();

    {
        let ctx = ctx.clone();
        logic.on_refresh_history(move || refresh_history(&ctx));
    }
    {
        // delete an entry + reclaim its partial store
        let ctx = ctx.clone();
        logic.on_delete_row(move |id: SharedString| {
            let ctx_bg = ctx.clone();
            ctx.rt.spawn_blocking(move || {
                // A running transfer still uses its partial store.
                let running = ctx_bg.history.list().ok().is_some_and(|records| {
                    records.iter().any(|r| {
                        r.id == id.as_str() && matches!(r.status, TransferStatus::InProgress)
                    })
                });
                if running {
                    platform::toast_later(
                        &ctx_bg.weak,
                        "That transfer is still running. Stop it first.",
                        true,
                    );
                    return;
                }
                match ctx_bg.history.delete(id.as_ref()) {
                    Ok(Some(record)) => {
                        let temp_dir = crate::engine::storage::temp_dir();
                        let partial = record.resumable_store_path.as_deref().map(PathBuf::from);
                        if partial.is_some_and(|p| is_reclaimable_partial(&p, &temp_dir)) {
                            reclaim_partial(&record, &temp_dir);
                        }
                    }
                    Ok(None) => {}
                    Err(e) => tracing::warn!("failed to delete history row: {e}"),
                }
                let _ = slint::invoke_from_event_loop(move || {
                    refresh_history(&ctx_bg);
                });
            });
        });
    }
    {
        // Tap a row: open what it received; pasted text is shared (Android)
        // or copied (desktop).
        let ctx = ctx.clone();
        logic.on_open_row(move |id: SharedString| {
            let ctx_bg = ctx.clone();
            ctx.rt.spawn_blocking(move || match row_target(&ctx_bg, &id) {
                Some(RowTarget::Text(text)) => {
                    #[cfg(target_os = "android")]
                    platform::share_text(&text);
                    #[cfg(not(target_os = "android"))]
                    let _ = ctx_bg.weak.upgrade_in_event_loop(move |ui| {
                        match copy_to_clipboard(&text) {
                            Ok(()) => toast(&ui, "Text copied", false),
                            Err(e) => toast(&ui, &format!("Could not copy: {e}"), true),
                        }
                    });
                }
                Some(RowTarget::Path(path)) => open_path(&path.to_string_lossy()),
                None => platform::toast_later(&ctx_bg.weak, "The files are gone", true),
            });
        });
    }
    {
        // Android: hand a row's files or text to another app.
        let ctx = ctx.clone();
        logic.on_share_row(move |id: SharedString| {
            let ctx_bg = ctx.clone();
            ctx.rt.spawn_blocking(move || match row_target(&ctx_bg, &id) {
                #[cfg(target_os = "android")]
                Some(RowTarget::Text(text)) => platform::share_text(&text),
                #[cfg(target_os = "android")]
                Some(RowTarget::Path(path)) => platform::share_path(&path),
                #[cfg(not(target_os = "android"))]
                Some(_) => {}
                None => platform::toast_later(&ctx_bg.weak, "The files are gone", true),
            });
        });
    }
}

/// What tapping a history row acts on.
enum RowTarget {
    Text(String),
    Path(PathBuf),
}

/// The pasted text of a row, else the received file or folder (falling back
/// to the folder it was saved into) while it still exists.
fn row_target(ctx: &AppCtx, id: &str) -> Option<RowTarget> {
    let records = ctx.history.list().ok()?;
    let record = records.iter().find(|r| r.id == id)?;
    if let Some(text) = &record.text {
        return Some(RowTarget::Text(text.clone()));
    }
    let dir = PathBuf::from(record.save_path.as_deref()?);
    let name = if record.root_name.is_empty() {
        record.file_names.first()
    } else {
        Some(&record.root_name)
    };
    name.map(|name| dir.join(name))
        .filter(|path| path.exists())
        .or_else(|| dir.is_dir().then_some(dir))
        .map(RowTarget::Path)
}

fn register_settings(ctx: &AppCtx) {
    let Some(ui) = ctx.weak.upgrade() else {
        return;
    };
    let logic = ui.global::<Logic>();

    {
        let ctx = ctx.clone();
        logic.on_pick_downloads_dir(move || {
            let ctx_bg = ctx.clone();
            let rt_here = ctx_bg.rt.clone();
            rt_here.spawn_blocking(move || {
                if let Some(folder) = pick_downloads_folder() {
                    let text = folder.to_string_lossy().into_owned();
                    let weak = ctx_bg.weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            ui.global::<State>().set_downloads_dir(text.into());
                        }
                    });
                }
            });
        });
    }

    {
        // Name ourselves
        let ctx = ctx.clone();
        logic.on_rename_self_name(move || {
            let Some(node) = ctx.node() else {
                return;
            };
            let name = {
                let Some(ui) = ctx.weak.upgrade() else { return };
                ui.global::<State>().get_name_input().trim().to_string()
            };
            if name.is_empty() {
                return;
            }
            let weak = ctx.weak.clone();
            let ctx_bg = ctx.clone();
            ctx.rt.spawn(async move {
                match node.set_device_display_name(&name) {
                    Ok(info) => {
                        let weak2 = weak.clone();
                        let ctx_bg2 = ctx_bg.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = weak2.upgrade() {
                                let state = ui.global::<State>();
                                state.set_my_name(info.display_name.clone().into());
                                state.set_name_input(info.display_name.into());
                                state.set_name_editing(false);
                                toast(&ui, "Device name saved", false);
                            }
                        });
                        refresh_peers(&ctx_bg2);
                    }
                    Err(e) => {
                        let msg = format!("Could not set name: {e}");
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = weak.upgrade() {
                                toast(&ui, &msg, true);
                            }
                        });
                    }
                }
            });
        });
    }

    {
        let ctx = ctx.clone();
        logic.on_save_settings(move || {
            let new_settings = {
                let Some(ui) = ctx.weak.upgrade() else { return };
                let state = ui.global::<State>();
                settings_from_state(&state)
            };

            *ctx.settings.lock().unwrap() = new_settings.clone();
            let path = ctx.settings_path.clone();
            let weak = ctx.weak.clone();
            let ctx_bg = ctx.clone();
            ctx.rt
                .spawn_blocking(move || match new_settings.save(&path) {
                    Ok(()) => {
                        let weak = weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = weak.upgrade() {
                                let state = ui.global::<State>();
                                state.set_settings_status("Saved.".into());
                                state.set_settings_failed(false);
                                toast(&ui, "Settings saved", false);
                            }
                        });
                        let Some(node) = ctx_bg.node() else {
                            return;
                        };
                        let (relay, discovery, disc) = {
                            let cfg = ctx_bg.settings.lock().unwrap().clone();
                            (
                                cfg.relay_mode(),
                                cfg.discovery_mode(),
                                cfg.discoverability(),
                            )
                        };
                        ctx_bg.rt.spawn(async move {
                            if let Err(e) = node.reconfigure_network(relay.into(), discovery).await
                            {
                                tracing::warn!("reconfigure failed: {e}");
                            }
                            if let Err(e) = node.set_discoverability(disc).await {
                                tracing::warn!("discoverability change failed: {e}");
                            }
                        });
                    }
                    Err(e) => {
                        let msg = format!("Could not save settings: {e}");
                        let weak = weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = weak.upgrade() {
                                let state = ui.global::<State>();
                                state.set_settings_status(msg.clone().into());
                                state.set_settings_failed(true);
                                toast(&ui, &msg, true);
                            }
                        });
                    }
                });
        });
    }

    {
        let ctx = ctx.clone();
        logic.on_test_relay(move || {
            let Some(ui) = ctx.weak.upgrade() else {
                return;
            };
            let arg = settings_from_state(&ui.global::<State>()).relay_config_arg();
            let weak = ctx.weak.clone();
            ctx.rt.spawn(async move {
                let _ = slint::invoke_from_event_loop({
                    let weak = weak.clone();
                    move || {
                        if let Some(ui) = weak.upgrade() {
                            let s = ui.global::<State>();
                            s.set_relay_testing(true);
                            s.set_relay_test_status("Testing…".into());
                            s.set_relay_test_failed(false);
                        }
                    }
                });
                let result = verify_relays(arg).await;
                let _ = slint::invoke_from_event_loop({
                    let weak = weak.clone();
                    move || {
                        if let Some(ui) = weak.upgrade() {
                            let s = ui.global::<State>();
                            s.set_relay_testing(false);
                            s.set_relay_test_failed(result.is_err());
                            match result {
                                Ok(resp) => {
                                    let msg = match resp.url {
                                        Some(url) => {
                                            format!("Connected to {url} ({}ms)", resp.latency_ms)
                                        }
                                        None => format!("Connected ({}ms)", resp.latency_ms),
                                    };
                                    s.set_relay_test_status(msg.into());
                                    toast(&ui, "Relay connection verified", false);
                                }
                                Err(e) => {
                                    let msg = format!("Relay check failed: {e}");
                                    s.set_relay_test_status(msg.clone().into());
                                    toast(&ui, &msg, true);
                                }
                            }
                        }
                    }
                });
            });
        });
    }

    {
        let ctx = ctx.clone();
        logic.on_check_relay_status(move || {
            let Some(ui) = ctx.weak.upgrade() else {
                return;
            };
            let arg = settings_from_state(&ui.global::<State>()).relay_config_arg();
            let weak = ctx.weak.clone();
            ctx.rt.spawn(async move {
                match get_relay_status(Some(arg)).await {
                    Ok(resp) => {
                        let label = match resp.kind.as_str() {
                            "disabled" => "Relay disabled".to_string(),
                            "custom" => format!(
                                "Custom relay: {}",
                                resp.url.as_deref().unwrap_or("unreachable")
                            ),
                            "public" => {
                                format!("Public relay: {}", resp.url.as_deref().unwrap_or("n0"))
                            }
                            _ => "Relay unavailable".to_string(),
                        };
                        let fell_back = resp.fell_back_to_public;
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = weak.upgrade() {
                                let state = ui.global::<State>();
                                state.set_relay_status(label.into());
                                state.set_relay_status_failed(false);
                                if fell_back {
                                    toast(
                                        &ui,
                                        "Custom relay unreachable, using public relays",
                                        true,
                                    );
                                }
                            }
                        });
                    }
                    Err(e) => {
                        let msg = format!("Relay status failed: {e}");
                        let _ = slint::invoke_from_event_loop({
                            let weak = weak.clone();
                            move || {
                                if let Some(ui) = weak.upgrade() {
                                    let state = ui.global::<State>();
                                    state.set_relay_status(msg.clone().into());
                                    state.set_relay_status_failed(true);
                                    toast(&ui, &msg, true);
                                }
                            }
                        });
                    }
                }
            });
        });
    }

    {
        let ctx = ctx.clone();
        logic.on_test_discovery(move || {
            let Some(ui) = ctx.weak.upgrade() else {
                return;
            };
            let arg = settings_from_state(&ui.global::<State>()).discovery_config_arg();
            let weak = ctx.weak.clone();
            ctx.rt.spawn(async move {
                let _ = slint::invoke_from_event_loop({
                    let weak = weak.clone();
                    move || {
                        if let Some(ui) = weak.upgrade() {
                            let s = ui.global::<State>();
                            s.set_discovery_testing(true);
                            s.set_discovery_test_status("Testing…".into());
                            s.set_discovery_test_failed(false);
                        }
                    }
                });
                let result = verify_discovery(arg).await;
                let _ = slint::invoke_from_event_loop({
                    let weak = weak.clone();
                    move || {
                        if let Some(ui) = weak.upgrade() {
                            let s = ui.global::<State>();
                            s.set_discovery_testing(false);
                            s.set_discovery_test_failed(result.is_err());
                            match result {
                                Ok(resp) => {
                                    let msg = match resp.url {
                                        Some(url) => format!(
                                            "Discovery server reachable: {url} ({}ms)",
                                            resp.latency_ms
                                        ),
                                        None => format!("Reachable ({}ms)", resp.latency_ms),
                                    };
                                    s.set_discovery_test_status(msg.into());
                                    toast(&ui, "Discovery server verified", false);
                                }
                                Err(e) => {
                                    let msg = format!("Discovery check failed: {e}");
                                    s.set_discovery_test_status(msg.clone().into());
                                    toast(&ui, &msg, true);
                                }
                            }
                        }
                    }
                });
            });
        });
    }

    {
        let ctx = ctx.clone();
        logic.on_page_changed(move |page: SharedString| {
            let page = page.to_string();
            if page == "peer" {
                refresh_peers(&ctx);
                refresh_suggestions(&ctx);
                refresh_history(&ctx);
            } else if page == "add-peer" {
                refresh_suggestions(&ctx);
                refresh_peers(&ctx);
            } else if page == "settings" {
                if let Some(ui) = ctx.weak.upgrade() {
                    ui.global::<State>().set_name_editing(false);
                }
            }
        });
    }
}

/// Shared startup: settings, history store, node service and UI wiring.
/// Called from `main()` on desktop and `android_main()` on Android.
pub fn run() {
    platform::init_logging();
    #[cfg(target_os = "android")]
    crate::android::use_cache_dir_for_blob_stores();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    let data_dir = std::env::var("FLIPFLOP_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| platform::default_data_dir());
    let _ = std::fs::create_dir_all(&data_dir);

    let settings_path = data_dir.join("settings.json");
    let settings = Arc::new(Mutex::new(Settings::load(&settings_path)));

    let history = Arc::new(TransferHistoryStore::new(&data_dir));
    if let Err(e) = history.mark_interrupted() {
        tracing::warn!("history interrupt sweep failed: {e}");
    }

    let ui = AppWindow::new().expect("failed to create AppWindow");
    let state = ui.global::<State>();
    settings_into_state(&state, &settings.lock().unwrap());
    state.set_my_name("…".into());
    // Responsive layout flags: single-pane + bottom nav on touch devices.
    state.set_touch(platform::TOUCH);
    state.set_compact(platform::TOUCH);
    transfers::init_model(&ui);

    let ctx = AppCtx {
        weak: ui.as_weak(),
        rt: rt.handle().clone(),
        history,
        settings,
        settings_path,
        node: Arc::new(Mutex::new(None)),
        pair_requests: Arc::new(Mutex::new(Vec::new())),
        queue: MainQueue::default(),
        transfers: Transfers::default(),
    };

    // Android "intent listener": content shared into the app ("Send to
    // flipflop") is queued, and the peer list opens so the user can pick
    // who gets it. This fires for the launch intent and for every later
    // share, including ones made while the app is running.
    #[cfg(target_os = "android")]
    {
        let ctx_shared = ctx.clone();
        crate::android::on_shared(move |staged, origin| {
            transfers::refresh_outbox(&ctx_shared);
            let Some(ui) = ctx_shared.weak.upgrade() else {
                return;
            };
            if staged.is_empty() {
                return;
            }
            let what = if staged.len() == 1 {
                "1 file".to_string()
            } else {
                format!("{} files", staged.len())
            };
            // Picked files: the user is already where they want to send from.
            if origin == crate::android::Origin::Picker {
                toast(&ui, &format!("{what} ready to send"), false);
                return;
            }
            let state = ui.global::<State>();
            state.set_page("peer".into());
            state.set_peer_open(false);
            toast(
                &ui,
                &format!("{what} ready. Pick a peer to send to"),
                false,
            );
        });
        crate::android::stage_launch_intent();
    }
    ui.global::<Logic>()
        .on_back_at_root(crate::android::move_to_background);
    #[cfg(target_os = "android")]
    ui.global::<Logic>()
        .on_pick_files(crate::android::pick_files);

    register_node(&ctx);
    register_peers(&ctx);
    register_add_peer(&ctx);
    register_history(&ctx);
    register_settings(&ctx);
    transfers::register(&ctx);

    ui.run().expect("UI error");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(name: &str, device_type: &str, os: &str) -> PairedDeviceInfo {
        serde_json::from_value(serde_json::json!({
            "endpoint_id": "ABCDEF0123456789",
            "display_name": name,
            "device_type": device_type,
            "os": os,
            "paired_at": 0,
            "last_seen_at": 0,
            "online": true,
        }))
        .unwrap()
    }

    #[test]
    fn initials_take_first_letters_of_two_words() {
        assert_eq!(initials("Living Room TV"), "LR");
        assert_eq!(initials("laptop"), "L");
        assert_eq!(initials("  (work) phone "), "WP");
        assert_eq!(initials(""), "");
    }

    #[test]
    fn relay_urls_split_on_lines_commas_and_spaces() {
        assert_eq!(
            split_urls("https://a.example\n https://b.example, https://c.example\n\n"),
            [
                "https://a.example",
                "https://b.example",
                "https://c.example"
            ]
        );
        assert!(split_urls("  \n").is_empty());
    }

    #[test]
    fn paired_row_formats_detail_and_lowercases_id() {
        let row = paired_row(&device("Desk", "desktop", "linux"));
        assert_eq!(row.endpoint_id.as_str(), "abcdef0123456789");
        assert_eq!(row.name.as_str(), "Desk");
        assert_eq!(row.initials.as_str(), "D");
        assert_eq!(row.detail.as_str(), "desktop · linux · ABCDEF01");
    }

    #[test]
    fn paired_row_without_name_or_metadata() {
        let row = paired_row(&device(" ", "", ""));
        assert_eq!(row.name.as_str(), "ABCDEF01");
        assert_eq!(row.detail.as_str(), "ABCDEF01");
    }

    #[test]
    fn history_row_summarises_record() {
        let mut record = TransferRecord::new(TransferDirection::Receive, String::new(), 2048);
        record.status = TransferStatus::Cancelled;
        record.item_count = 3;
        record.file_names = vec!["a".into(), "b".into(), "c".into()];
        record.save_path = Some("/dl/peer".into());
        record.conflict_count = 1;

        let row = row_from_record(&record);
        assert_eq!(row.title.as_str(), "a");
        assert_eq!(row.direction.as_str(), "receive");
        assert_eq!(
            (row.status.as_str(), row.tone.as_str()),
            ("Cancelled", "muted")
        );
        assert_eq!(row.detail.as_str(), "3 items · /dl/peer · 1 renamed");
        assert_eq!(row.size.as_str(), "2.0 KB");
        assert_eq!(row.speed.as_str(), "-");
        assert!(row.can_open);
    }
}
