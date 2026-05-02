//! Library crate root.
//!
//! Mirrors the module declarations in `src/main.rs` so that `cargo test --lib`
//! can run the unit-test suite defined inside each module (notably the
//! evaluator's `cargo test --lib harness::gemini`). The binary at
//! `src/main.rs` keeps its own module tree unchanged; this root simply
//! re-declares the same modules as `pub` under the library crate so the
//! library target also compiles them.
//!
//! The duplication does compile each module twice (once as part of the
//! library, once as part of the binary), but that is transparent to
//! callers and keeps the binary-target diff minimal.

pub mod app;
pub mod banner;
pub mod buffer;
pub mod chat_adapters;
pub mod command;
pub mod config;
pub mod delivery;
pub mod harness;
pub mod power;
pub mod session;
pub mod socket;
pub mod state_persistor;
pub mod state_store;
pub mod structured_output;
pub mod tmux;
