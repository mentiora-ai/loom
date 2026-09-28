//! `receipt_marshaller` — see crate root.
mod canonical_bytes;
mod cookies_canonical;
pub mod receipt_marshaller;
pub use cookies_canonical::cookie_read_outcome_hash;
pub use receipt_marshaller::*;

#[cfg(test)]
mod interface_tests;
