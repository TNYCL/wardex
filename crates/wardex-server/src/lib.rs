//! The Wardex daemon: Unix socket API for the CLI, HTTP (`/healthz`, MCP in
//! Phase 2, always behind a token) and the webhook dispatcher.

pub mod api;
pub mod daemon;
pub mod http;
pub mod socket;
pub mod webhook;

pub use socket::Client;
