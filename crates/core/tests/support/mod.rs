//! Shared fixtures for integration tests. Each test binary that needs one declares
//! `mod support;` and uses only what it needs.
#![allow(dead_code)]

pub mod fake_mysql;
