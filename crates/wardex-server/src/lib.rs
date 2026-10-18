//! The Wardex daemon: Unix socket API for the CLI, HTTP (`/healthz` and the
//! MCP endpoint `/mcp`, always behind a token) and the webhook dispatcher.

pub mod api;
pub mod daemon;
pub mod http;
pub mod mcp;
pub mod socket;
pub mod webhook;

pub use socket::Client;
