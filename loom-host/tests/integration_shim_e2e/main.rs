//! Full host→shim end-to-end integration test.
//!
//! Drives `ShimManager::send` against the real `loom-shim-chromium`
//! binary, which spawns the test-only `fake-chromium` binary that
//! simulates a real Chromium DevTools endpoint. Validates the chain:
//!
//!   ShimManager.send(ShimId, opaque_cbor)
//!     → spawn loom-shim-chromium child via socketpair + LOOM_SHIM_FD=3
//!     → loom-shim-chromium spawns fake-chromium subprocess
//!     → fake-chromium prints "DevTools listening on ws://127.0.0.1:N/.."
//!     → loom-shim-chromium's ChromiumSupervisor::start parses the URL
//!     → ChromiumCdpConnection connects via tokio-tungstenite
//!     → ShimDispatcher routes ShimRequest::CdpSend → cdp.command
//!     → fake-chromium responds with canned JSON
//!     → ShimResponse::Ok flows back through the demux loop
//!     → ShimManager::send returns the re-encoded payload bytes
//!
//! Run:
//!   `cargo build -p loom-shims --features fake-chromium-bin --bin fake-chromium`
//!   `cargo test -p loom-host --test integration_shim_e2e -- --ignored`
//!
//! Marked `#[ignore]` so a default `cargo test --workspace` doesn't
//! force the fake-chromium build. The ignore is opt-in so CI can run
//! it after building the harness binary.

#![cfg(unix)]

mod audio;
mod capture;
mod common;
mod input;
mod round_trip;
mod screencast;
