//! `host_function_table` — see crate root.
pub mod host_function_table;
pub use host_function_table::*;

mod chromium_registration;
mod host_impl;
mod navigate_verb;
mod net_guard;
mod page_verbs;

#[cfg(test)]
mod interface_tests;
