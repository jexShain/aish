//! The product-facing frame model.
//!
//! A frame is two channels, never one:
//!
//! * [`HistoryBatch`] — rows that are **finished** and must leave the live
//!   area: they are written once, top-down, pushing older rows into the
//!   terminal's own scrollback. Batches carry a monotonically increasing
//!   [`HistoryBatch::id`]; the renderer acks an id only after the bytes are on
//!   the wire, and ignores ids it already acked.
//! * [`Frame::viewport`] — a complete mirror of the mutable area, re-supplied
//!   in full on every frame. The renderer diffs it against the previous frame
//!   and rewrites only the rows whose text changed.
//!
//! Contract invariants (enforced by the renderer, relied on by products):
//!
//! 1. History finality is a product decision. The renderer never infers that a
//!    row is complete because it scrolled off the screen.
//! 2. History is append-only, carries monotonic ids, and its rows are released
//!    only after the renderer acks the batch.
//! 3. Every paint is atomic (hidden cursor + synchronized output + autowrap
//!    disabled for the whole write), never line-at-a-time flushing.
//! 4. Top rows are never deleted: terminals such as Terminal.app archive rows
//!    that disappear from the top into scrollback, so deleting there would
//!    corrupt history.
//! 5. Component output is dirtied by reference equality of its cached render
//!    result, not by deep-diffing strings.

use crate::render::PreparedLine;

/// Finished rows to commit into scrollback, as one atomic unit.
///
/// The product hands out ids `n, n+1, n+2, ...`. Ids may be skipped (an empty
/// batch is usually not submitted at all) but never reordered: the renderer
/// writes the rows of the highest unseen id and acks it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HistoryBatch {
    /// Monotonic batch id, strictly greater than every previously acked id.
    pub id: u64,
    /// Rows to commit, oldest first.
    pub rows: Vec<PreparedLine>,
}

impl HistoryBatch {
    /// Build a batch from already-prepared rows.
    pub fn new(id: u64, rows: Vec<PreparedLine>) -> Self {
        Self { id, rows }
    }

    /// Whether committing this batch would add nothing visible.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

/// One product frame: at most one history batch plus the whole viewport.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Frame {
    /// Newly finished rows, or `None` when nothing was committed.
    pub history: Option<HistoryBatch>,
    /// Complete mirror of the live area, top row first.
    pub viewport: Vec<PreparedLine>,
}

impl Frame {
    /// A frame that only refreshes the viewport.
    pub fn viewport(viewport: Vec<PreparedLine>) -> Self {
        Self {
            history: None,
            viewport,
        }
    }
}
