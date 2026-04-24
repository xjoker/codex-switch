// Re-export modules needed by integration tests.
// The binary entry point remains in main.rs.
#![allow(dead_code)]

pub mod auth;
mod cache;
mod cli;
mod color;
pub mod config;
mod daemon;
mod error;
pub mod jwt;
mod login;
mod output;
pub mod profile;
mod tui;
mod update;
pub mod usage;
mod warmup;
