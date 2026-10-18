//! Drivers that spawn and talk to agent CLIs and translate their native output
//! into [`wardex_core::Event`]s. Protocol details: `docs/drivers.md`.

pub mod claude;
pub mod runner;

pub use claude::ClaudeDriver;
