//! Completion sources: the token under the cursor in, ranked candidates out.
//!
//! The REPL does not know where candidates come from. A [`CompletionSource`] is
//! anything that can answer "what could this token become", which keeps command
//! names, file names and future providers behind the same testable seam.
//!
//! Two contracts matter to callers:
//!
//! * [`token_range`] is the single definition of "the token under the cursor",
//!   shared by completion and ghost text so a suggestion can never be computed
//!   for a different span than the replacement it is applied to.
//! * A source returns at most [`MAX_CANDIDATES`] candidates, so a thousand-entry
//!   `PATH` cannot turn one Tab into an unbounded allocation.

use std::ops::Range;

/// Upper bound on candidates a source returns.
pub const MAX_CANDIDATES: usize = 50;

/// One completion candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// Text shown in a completion menu.
    pub display: String,
    /// Text that replaces the token under the cursor.
    pub replacement: String,
    /// Optional one-line explanation, shown dimmed next to `display`.
    pub description: Option<String>,
}

impl Candidate {
    /// Candidate whose display and replacement are the same string.
    pub fn new(replacement: impl Into<String>) -> Self {
        let replacement = replacement.into();
        Self {
            display: replacement.clone(),
            replacement,
            description: None,
        }
    }
}

/// Anything that can propose replacements for the token under the cursor.
pub trait CompletionSource: Send + Sync {
    /// Candidates for the token ending at `cursor` (a byte offset into `line`,
    /// clamped when out of range). The returned candidates are ordered by
    /// preference; an empty vector means "nothing to suggest".
    fn complete(&self, line: &str, cursor: usize) -> Vec<Candidate>;
}

/// Byte range of the whitespace-delimited token that ends at `cursor`.
///
/// The token is the run of non-whitespace characters immediately before the
/// cursor, so completion on `git sta|` proposes for `sta` and leaves `git `
/// alone. `cursor` is clamped to the buffer length and to a character boundary,
/// and the range is always a slice of whitespace-free text.
pub fn token_range(line: &str, cursor: usize) -> Range<usize> {
    let mut end = cursor.min(line.len());
    while end > 0 && !line.is_char_boundary(end) {
        end -= 1;
    }
    let start = line[..end]
        .char_indices()
        .rev()
        .find(|(_, ch)| ch.is_whitespace())
        .map(|(idx, ch)| idx + ch.len_utf8())
        .unwrap_or(0);
    start..end
}

/// Longest common prefix of `values`, compared character by character.
pub fn common_prefix<'a>(values: impl IntoIterator<Item = &'a str>) -> String {
    let mut iter = values.into_iter();
    let Some(first) = iter.next() else {
        return String::new();
    };
    let mut prefix = first.to_string();
    for value in iter {
        while !value.starts_with(prefix.as_str()) {
            if prefix.pop().is_none() {
                return prefix;
            }
        }
    }
    prefix
}

/// Completion over a fixed word list.
///
/// Filtering is a case-sensitive prefix match against the token under the
/// cursor. Ranking puts an exact match first (so pressing Tab on a complete
/// word keeps it) and otherwise orders the matches lexicographically, which is
/// also the order the menu displays.
#[derive(Debug, Clone, Default)]
pub struct WordListSource {
    /// Sorted and deduplicated words.
    words: Vec<String>,
}

impl WordListSource {
    /// Build a source over `words` (`""` entries are dropped).
    pub fn new(words: Vec<String>) -> Self {
        let mut words: Vec<String> = words.into_iter().filter(|word| !word.is_empty()).collect();
        words.sort_unstable();
        words.dedup();
        Self { words }
    }

    /// Number of distinct words.
    pub fn len(&self) -> usize {
        self.words.len()
    }

    /// Whether the source holds no words.
    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }
}

impl CompletionSource for WordListSource {
    fn complete(&self, line: &str, cursor: usize) -> Vec<Candidate> {
        let range = token_range(line, cursor);
        let token = &line[range.clone()];
        let mut matched: Vec<&String> = self
            .words
            .iter()
            .filter(|word| word.starts_with(token))
            .collect();
        // `words` is sorted, so the lexicographic order is already in place;
        // an exact match only has to jump to the front.
        if let Some(index) = matched.iter().position(|word| word.as_str() == token) {
            let exact = matched.remove(index);
            matched.insert(0, exact);
        }
        matched
            .into_iter()
            .take(MAX_CANDIDATES)
            .map(|word| Candidate::new(word.as_str()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(list: &[&str]) -> Vec<String> {
        list.iter().map(|word| word.to_string()).collect()
    }

    #[test]
    fn prefix_filtering() {
        let source = WordListSource::new(words(&["git", "gcc", "go", "ls", "GIT"]));
        let candidates = source.complete("g", 1);
        let replacements: Vec<&str> = candidates
            .iter()
            .map(|candidate| candidate.replacement.as_str())
            .collect();
        assert_eq!(replacements, ["gcc", "git", "go"]);

        // Case-sensitive: an uppercase word is not a match for a lowercase token.
        assert!(source
            .complete("G", 1)
            .iter()
            .all(|candidate| candidate.replacement == "GIT"));

        // Only the token under the cursor is filtered on.
        let candidates = source.complete("git st", 6);
        assert!(candidates.is_empty());
        let candidates = source.complete("ls gi", 5);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].replacement, "git");
    }

    #[test]
    fn max_candidates() {
        let many: Vec<String> = (0..60).map(|index| format!("cmd{index:02}")).collect();
        let source = WordListSource::new(many);
        let candidates = source.complete("cmd", 3);
        assert_eq!(candidates.len(), MAX_CANDIDATES);
        assert_eq!(candidates[0].replacement, "cmd00");
    }

    #[test]
    fn empty_prefix_returns_all() {
        let source = WordListSource::new(words(&["beta", "alpha", "gamma"]));
        let candidates = source.complete("", 0);
        let replacements: Vec<&str> = candidates
            .iter()
            .map(|candidate| candidate.replacement.as_str())
            .collect();
        assert_eq!(replacements, ["alpha", "beta", "gamma"]);
        assert_eq!(candidates.len(), source.len());
    }

    #[test]
    fn exact_match_ranks_first() {
        let source = WordListSource::new(words(&["git", "git-add", "git-log"]));
        let candidates = source.complete("git", 3);
        assert_eq!(candidates[0].replacement, "git");
        assert_eq!(candidates.len(), 3);
    }

    #[test]
    fn token_range_handles_boundaries() {
        // Cursor inside a multi-byte scalar clamps back to that scalar's start,
        // which is *after* the separating space: no partial token is offered.
        let line = "echo 漢";
        let range = token_range(line, line.len() - 1);
        assert_eq!(range, 5..5);
        // Past the scalar, the whole word is the token.
        assert_eq!(&line[token_range(line, line.len())], "漢");
        assert_eq!(token_range("   ", 3), 3..3);
        assert_eq!(token_range("a\nb", 3), 2..3);
        assert_eq!(token_range("x", 99), 0..1);
    }

    #[test]
    fn common_prefix_shortens_to_all() {
        assert_eq!(common_prefix(["git-add", "git-log"]), "git-");
        assert_eq!(common_prefix(["alpha", "beta"]), "");
        assert_eq!(common_prefix(["single"]), "single");
        assert_eq!(common_prefix(Vec::<&str>::new()), "");
    }
}
