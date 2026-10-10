//! # `engine`, stable public API for the app and integration tests
//!
//! ## Canonical imports
//!
//! Transfers go through [`NodeService`]: `share_with_peer` serves files from
//! the node's own endpoint and `download_from_peer` fetches them over it.
//!
//! Desktop/mobile builds use the `native` platform crate (re-exported here).

pub use crate::native::*;

pub use crate::protocol::identity::unix_now_ms;
/// Shared protocol helpers not re-exported by the platform crates.
pub use crate::protocol::{
    allows_unpaired_control, build_relay_mode, download_to_store, get_relay_status,
    pairing_host_is_persistent, relay_fallback_policy, resolve_relay_mode_with_fallback,
    sanitize_folder_name, should_answer_identity, should_publish_mdns,
    should_run_background_presence, sign_challenge, unpaired_message_allowed, verify_challenge,
    verify_relays, ControlMessage, Discoverability, DownloadToStoreResult, PairedDevice,
    PairingStatus, PairingTicket, RelayConfigArg, RelayFallbackPolicy, RelayStatusResponse,
    VerifyRelaysResponse,
};
