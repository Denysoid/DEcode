#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![recursion_limit = "256"]

pub mod agent;
pub mod api;
pub mod attachments;
pub mod clipboard;
pub mod code_index;
pub mod config;
pub mod error;
pub mod github;
pub mod lsp;
pub mod managed_connections;
pub mod mcp;
pub mod notice;
pub mod onboarding;
pub mod parser;
pub mod plugins;
pub mod privacy;
pub mod redaction;
pub mod telemetry;
pub mod terminal;
pub mod tools;
pub mod ui;
pub mod usage;
