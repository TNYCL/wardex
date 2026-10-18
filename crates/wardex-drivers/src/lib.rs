//! Drivers that spawn and talk to agent CLIs and translate their native output
//! into [`wardex_core::Event`]s.
//!
//! Planned (see `docs/SCOPE.md`):
//! - `claude`: `claude -p` with stream-json input/output.
//! - `codex`: `codex app-server` over stdio JSON-RPC.
