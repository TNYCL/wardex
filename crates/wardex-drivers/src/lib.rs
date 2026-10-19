//! Drivers that spawn and talk to agent CLIs and translate their native output
//! into [`wardex_core::Event`]s, and read-only readers of their session stores
//! (`*_store`). Protocol details: `docs/drivers.md`; stores:
//! `docs/external-sessions.md`.

pub mod claude;
pub mod claude_store;
pub mod codex;
pub mod codex_store;
pub mod runner;

pub use claude::ClaudeDriver;
pub use claude_store::ClaudeStore;
pub use codex::CodexDriver;
pub use codex_store::CodexStore;
