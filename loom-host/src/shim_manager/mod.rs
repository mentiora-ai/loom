//! `shim_manager` — see crate root.
mod cdp_roundtrip;
mod fill;
mod fill_prepare;
mod helpers;
mod input_dispatch;
mod locator_js;
mod locator_resolve;
pub mod process;
mod senders;
mod senders_media;
pub mod shim_manager;
mod trusted_input;
mod types;
pub use locator_js::locator_element_js;
pub use shim_manager::*;

#[cfg(test)]
mod cdp_test_support;
#[cfg(test)]
mod interface_tests;
