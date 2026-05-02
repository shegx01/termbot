//! Shared test fixtures and mocks for integration tests under `tests/`.
//!
//! `cargo` does not compile files in `tests/<dir>/` as separate test
//! binaries — only top-level `tests/*.rs` are. Each test file `mod common;`
//! to bring this module into its compilation unit.

#![allow(dead_code)]

pub mod fixtures;
pub mod mocks;
