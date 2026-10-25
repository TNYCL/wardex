//! The Wardex daemon: Unix socket API for the CLI, HTTP (`/healthz` and the
//! MCP endpoint `/mcp`, always behind a token) and the webhook dispatcher.

pub mod api;
mod callback;
pub mod daemon;
pub mod http;
pub mod mcp;
mod mcp_compat;
pub mod mcp_events;
pub mod socket;
pub mod webhook;

pub use socket::Client;
