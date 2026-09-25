//! FastTran core library.
//!
//! The desktop UI lives in `app`, while the networking and transfer code is
//! kept in small modules so it can be tested without starting a window.

#[cfg(target_os = "android")]
mod android_bridge;
pub mod app;
pub mod config;
pub mod discovery;
pub mod format;
pub mod model;
pub mod network;
pub mod protocol;
pub mod receiver;
pub mod sender;

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(app: android_activity::AndroidApp) {
    if let Err(error) = app::run_android(app) {
        eprintln!("FastTran Android startup failed: {error}");
    }
}
