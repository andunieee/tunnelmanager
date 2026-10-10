//! Shared UI + logic for the Slint frontend.
//!
//! The same code builds as the desktop binary (`flipflop`) and as
//! the cdylib loaded by the Android `NativeActivity` (see `android.rs` and
//! docs/development.md, Android section).

pub mod android;
pub mod app;
pub mod emitter;
pub mod engine;
pub mod format;
pub mod native;
pub mod protocol;
pub mod platform;
pub mod qr;
pub mod recorder;
pub mod settings;
pub mod transfers;

slint::include_modules!();
