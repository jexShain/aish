//! Component tree with reference-identity memoization.
//!
//! A [`Component`] renders itself to plain rows at a given width. Composites
//! ([`Container`]) must not re-run their children on every frame — a spinner in
//! one row should not cost the whole tree. The convention (invariant 5 of the
//! rendering model) is that a component signals "nothing changed" by returning
//! the *same* `Arc` its last render produced; the composite compares pointer
//! identity and reuses the previously joined rows instead of diffing strings.

use std::sync::Arc;

use crossterm::event::Event;

/// What a component did with an input event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputHandled {
    /// Not mine: let the parent or the next handler try.
    Ignored,
    /// Consumed, but the screen does not need repainting.
    Consumed,
    /// Consumed; the tree should repaint.
    RequestRender,
    /// The user asked to submit the current content.
    Submit,
    /// The user asked to cancel (Esc, Ctrl+C, ...).
    Cancel,
}

impl InputHandled {
    /// Whether the event implies a repaint.
    pub fn wants_render(self) -> bool {
        matches!(
            self,
            InputHandled::RequestRender | InputHandled::Submit | InputHandled::Cancel
        )
    }
}

/// One renderable piece of UI.
///
/// `render` is called at most once per frame per component (composites memoize
/// it) and must be pure: no I/O, no mutation of shared state visible to other
/// components. Returning a fresh `Vec<String>` is fine; the memo works on
/// [`Component::render_shared`].
pub trait Component {
    /// Rows this component occupies at `width` columns, top row first.
    ///
    /// Each row may contain ANSI sequences but must not contain newlines or
    /// tabs; the renderer normalizes them in
    /// [`prepare_line`](crate::render::prepare_line).
    fn render(&self, width: u16) -> Vec<String>;

    /// Memoized variant used by composites.
    ///
    /// The default implementation allocates a fresh `Arc` every call, which is
    /// always correct but gives composites nothing to memoize. Leaf components
    /// that cache their output (spinners, static text) should override this and
    /// return the same `Arc` while nothing changed.
    fn render_shared(&self, width: u16) -> Arc<Vec<String>> {
        Arc::new(self.render(width))
    }
}

/// A component that accepts terminal input.
pub trait Interactive: Component {
    /// Handle one event, reporting what the tree should do next.
    fn handle_input(&mut self, ev: Event) -> InputHandled;
}

/// Vertical stack of child components.
///
/// The memo lives behind a [`parking_lot::Mutex`] so [`Component::render_shared`]
/// can keep its `&self` signature: a component tree is shared with the product
/// through `Arc`, and a render must not need exclusive access to the whole tree.
#[derive(Default)]
pub struct Container {
    children: Vec<Box<dyn Component>>,
    memo: parking_lot::Mutex<ContainerMemo>,
}

#[derive(Default)]
struct ContainerMemo {
    /// Width the cached join was computed for.
    width: Option<u16>,
    /// `render_shared` results of the children, in join order.
    child_rows: Vec<Arc<Vec<String>>>,
    /// The joined output, reused while every child memo stays pointer-identical.
    joined: Option<Arc<Vec<String>>>,
}

impl Container {
    /// Empty container.
    pub fn new() -> Self {
        Self::default()
    }

    /// Container with `children` in top-to-bottom order.
    pub fn with_children(children: Vec<Box<dyn Component>>) -> Self {
        Self {
            children,
            memo: parking_lot::Mutex::new(ContainerMemo::default()),
        }
    }

    /// Append a child. The memo is dropped because the row count changes.
    pub fn push(&mut self, child: Box<dyn Component>) {
        self.children.push(child);
        *self.memo.get_mut() = ContainerMemo::default();
    }

    /// Number of direct children.
    pub fn len(&self) -> usize {
        self.children.len()
    }

    /// Whether the container has no children.
    pub fn is_empty(&self) -> bool {
        self.children.is_empty()
    }

    /// Mutable access to one child, e.g. to dispatch input.
    pub fn child_mut(&mut self, index: usize) -> Option<&mut Box<dyn Component>> {
        self.children.get_mut(index)
    }

    /// Drop the memo so the next render re-runs every child.
    pub fn invalidate(&mut self) {
        *self.memo.get_mut() = ContainerMemo::default();
    }

    /// Visible row count at `width`.
    pub fn height(&self, width: u16) -> usize {
        self.render_shared(width).len()
    }
}

impl Component for Container {
    fn render(&self, width: u16) -> Vec<String> {
        let rows = self.render_shared(width);
        rows.as_ref().clone()
    }

    fn render_shared(&self, width: u16) -> Arc<Vec<String>> {
        let mut memo = self.memo.lock();
        let child_rows = self
            .children
            .iter()
            .map(|child| child.render_shared(width))
            .collect::<Vec<_>>();

        let reuse = memo.width == Some(width)
            && memo.joined.is_some()
            && memo.child_rows.len() == child_rows.len()
            // Pointer equality, not string equality: this is the whole contract.
            && memo
                .child_rows
                .iter()
                .zip(&child_rows)
                .all(|(prev, now)| Arc::ptr_eq(prev, now));

        if reuse {
            // The children were unchanged, but their current memos must still
            // be recorded: a child may return a new `Arc` holding identical
            // rows, and the next frame compares against *these* pointers.
            let joined = Arc::clone(memo.joined.as_ref().expect("reuse requires a join"));
            memo.child_rows = child_rows;
            return joined;
        }

        let total: usize = child_rows.iter().map(|rows| rows.len()).sum();
        let mut joined = Vec::with_capacity(total);
        for rows in &child_rows {
            joined.extend(rows.iter().cloned());
        }
        let joined = Arc::new(joined);
        memo.width = Some(width);
        memo.child_rows = child_rows;
        memo.joined = Some(Arc::clone(&joined));
        joined
    }
}

/// Flatten component output into viewport rows at `width`.
///
/// This is the bridge from the component tree to [`crate::frame::Frame`]: rows
/// are normalized (ANSI preserved, tabs expanded, CJK-correct truncation) so the
/// renderer can address each one absolutely.
pub fn render_lines_to_viewport(
    rows: impl IntoIterator<Item = String>,
    width: u16,
) -> Vec<crate::render::PreparedLine> {
    rows.into_iter()
        .map(|row| crate::render::prepare_line(&row, width))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Leaf that memoizes its rows per width and counts actual renders.
    struct CountingLeaf {
        label: String,
        calls: AtomicUsize,
        memo: parking_lot::Mutex<Option<(u16, Arc<Vec<String>>)>>,
    }

    impl CountingLeaf {
        fn new(label: &str) -> Self {
            Self {
                label: label.to_string(),
                calls: AtomicUsize::new(0),
                memo: parking_lot::Mutex::new(None),
            }
        }
    }

    impl Component for CountingLeaf {
        fn render(&self, width: u16) -> Vec<String> {
            vec![format!("{} {}", self.label, width)]
        }

        fn render_shared(&self, width: u16) -> Arc<Vec<String>> {
            let mut memo = self.memo.lock();
            if let Some((cached_width, rows)) = memo.as_ref() {
                if *cached_width == width {
                    return Arc::clone(rows);
                }
            }
            self.calls.fetch_add(1, Ordering::Relaxed);
            let rows = Arc::new(self.render(width));
            *memo = Some((width, Arc::clone(&rows)));
            rows
        }
    }

    #[test]
    fn container_reuses_join_when_children_are_pointer_identical() {
        let container = Container::with_children(vec![
            Box::new(CountingLeaf::new("a")),
            Box::new(CountingLeaf::new("b")),
        ]);
        let first = container.render_shared(40);
        assert_eq!(first.len(), 2);
        let second = container.render_shared(40);
        assert!(Arc::ptr_eq(&first, &second), "join was rebuilt needlessly");
        assert_eq!(first.as_slice(), &["a 40".to_string(), "b 40".to_string()]);
    }

    #[test]
    fn container_rebuilds_on_width_change() {
        let container = Container::with_children(vec![Box::new(CountingLeaf::new("a"))]);
        let narrow = container.render_shared(10);
        let wide = container.render_shared(80);
        assert!(!Arc::ptr_eq(&narrow, &wide));
        assert_eq!(wide.as_slice(), &["a 80".to_string()]);
    }

    #[test]
    fn viewport_bridge_prepares_rows() {
        let rows = render_lines_to_viewport(["ab\tc".to_string(), "漢字".to_string()], 40);
        assert_eq!(rows[0].text, "ab      c");
        assert_eq!(rows[1].text, "漢字");
        assert_eq!(rows[1].width, 40);
    }
}
