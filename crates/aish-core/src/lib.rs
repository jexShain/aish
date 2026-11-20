// Suppress clippy lints that fire on Rust 1.95 stable but not on older versions.
#![allow(
    clippy::type_complexity,
    clippy::redundant_closure,
    clippy::match_like_matches_macro,
    clippy::option_as_ref_deref,
    clippy::field_reassign_with_default,
    clippy::len_zero,
    clippy::borrowed_box,
    clippy::new_without_default,
    clippy::needless_borrow,
    clippy::manual_strip,
    clippy::too_many_arguments
)]

pub mod audit;
pub mod error;
pub mod output;
pub mod plan;
pub mod types;

pub use audit::{AuditEvent, AuditEventType, AuditSink, NullAuditSink};
pub use error::{AishError, Result};
pub use output::{
    clear_global_sink, global_sink, install_global_sink, screen_owned, write_bytes, OutputSink,
    StdoutSink,
};
pub use plan::*;
pub use types::*;
