//! Toolbox core: the connection engine shared by the macOS app.
//!
//! Everything exposed to Swift goes through UniFFI. The Swift side owns all
//! rendering and windowing; this crate owns protocols, storage and secrets.

uniffi::setup_scaffolding!();

pub mod ad;
pub mod error;
pub mod onepassword;
pub mod rdp;
#[cfg(feature = "test-support")]
pub mod rdp_test_server;
pub mod runtime;
pub mod ssh;
pub mod store;
pub mod vault;

pub use error::CoreError;
pub use onepassword::ResolvedCredential;
pub use rdp::*;
pub use ssh::*;
pub use store::*;
pub use vault::*;

/// Version of the core library, surfaced in the app's About box.
#[uniffi::export]
pub fn core_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}
