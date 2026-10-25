//! Core building blocks of Wardex: the unified event model, configuration,
//! the SQLite store, the driver interface and the session manager.

pub mod config;
pub mod driver;
pub mod event;
pub mod external;
pub mod git;
pub mod glob;
pub mod manager;
pub mod process;
pub mod procscan;
pub mod session;
pub mod store;
pub mod time;

pub use config::Config;
pub use event::{
    Answer, DriverKind, Event, EventKind, GitState, QuestionItem, QuestionOption, StuckReason,
    ToolStatus,
};
pub use session::{
    AdoptSpec, EndReason, ListFilter, RunSpec, SessionId, SessionOrigin, SessionRecord,
    SessionState,
};

pub mod mcp_events;
