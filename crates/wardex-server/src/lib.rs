//! The Wardex daemon.
//!
//! Planned (see `docs/SCOPE.md`):
//! - Unix socket API for the local CLI (no token).
//! - HTTP: `/healthz` and the MCP endpoint (bearer token always required).
//! - Webhook dispatcher with retry, backoff and HMAC signatures.
