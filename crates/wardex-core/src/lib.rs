//! Core building blocks of Wardex: the unified event model, session state
//! and (later) the session manager and SQLite store.

pub mod event;
pub mod session;

pub use event::{
    DriverKind, Event, EventKind, QuestionItem, QuestionOption, StuckReason, ToolStatus,
};
pub use session::{SessionId, SessionState};
