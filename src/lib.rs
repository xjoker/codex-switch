// Re-export modules needed by integration tests.
// The binary entry point remains in main.rs.
//
// Private modules below are consumed only by the binary target, so in this
// lib target their items look dead; allow dead_code per-module instead of
// crate-wide so the pub modules keep real dead-code detection.

pub mod auth;
#[allow(dead_code)]
mod cache;
#[allow(dead_code)]
mod cli;
#[allow(dead_code)]
mod color;
pub mod config;
#[allow(dead_code)]
mod daemon;
#[allow(dead_code)]
mod error;
#[allow(dead_code)]
mod http_retry;
pub mod jwt;
#[allow(dead_code)]
mod logging;
#[allow(dead_code)]
mod login;
#[allow(dead_code)]
mod output;
pub mod profile;
#[allow(dead_code)]
mod signals;
#[allow(dead_code)]
mod tui;
#[allow(dead_code)]
mod update;
pub mod usage;
#[allow(dead_code)]
mod warmup;
pub mod workspace;
