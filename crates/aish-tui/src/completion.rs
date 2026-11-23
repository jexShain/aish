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

/// Whether `line` is a bare slash command being typed.
///
/// True only for `/` followed by a run of non-whitespace with no further
/// argument: `/mode` is a command being picked, `/model fast` is arguments and
/// must not reopen the list.
pub fn is_slash_token(line: &str) -> bool {
    let Some(rest) = line.strip_prefix('/') else {
        return false;
    };
    !rest.contains(char::is_whitespace)
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

/// Completion over the shell's slash-command catalogue.
///
/// Only a token that starts with `/` produces candidates, so a plain word is
/// left to the command and path sources. A bare `/` lists the whole catalogue;
/// any longer token filters by prefix.
///
/// Candidate order is the catalogue's own order, not an alphabetical sort: the
/// catalogue groups commands by purpose, which is the order the menu displays
/// and the reason the frequently used commands come first.
#[derive(Debug, Clone, Default)]
pub struct SlashCommandSource {
    /// `(name with leading slash, description)`, deduplicated, catalogue order.
    entries: Vec<(String, String)>,
}

impl SlashCommandSource {
    /// Build a source over `entries`; names without the leading `/` get one,
    /// empty descriptions become `None` and repeated names keep the first row.
    pub fn new(entries: Vec<(String, String)>) -> Self {
        let mut seen = std::collections::HashSet::new();
        let entries = entries
            .into_iter()
            .filter(|(name, _)| !name.trim_start_matches('/').is_empty())
            .map(|(name, description)| (format!("/{}", name.trim_start_matches('/')), description))
            .filter(|(name, _)| seen.insert(name.clone()))
            .collect();
        Self { entries }
    }

    /// Number of distinct commands.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the source holds no commands.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl CompletionSource for SlashCommandSource {
    fn complete(&self, line: &str, cursor: usize) -> Vec<Candidate> {
        let range = token_range(line, cursor);
        let token = &line[range];
        if !token.starts_with('/') {
            return Vec::new();
        }
        self.entries
            .iter()
            .filter(|(name, _)| name.starts_with(token))
            .take(MAX_CANDIDATES)
            .map(|(name, description)| Candidate {
                display: name.clone(),
                replacement: name.clone(),
                description: (!description.is_empty()).then(|| description.clone()),
            })
            .collect()
    }
}

/// Maximum number of `PATH` executables offered as completions.
///
/// A real desktop `PATH` holds thousands of entries, more than a completion
/// source is worth scanning per keystroke.
const PATH_WORD_CAP: usize = 500;

/// Upper bound on directory entries inspected while building the vocabulary.
const SCAN_CAP: usize = 20_000;

/// Every executable name found on `PATH`, shortest names first.
///
/// Slash commands are not here: they come from [`SlashCommandSource`], which
/// knows the shell's real catalogue. The cap keeps the *shortest* names: that
/// is where the heavily used commands live (`ls`, `cd`, `git`), and a cutoff by
/// directory order would drop them for whichever packages happened to be read
/// last.
pub fn path_words() -> Vec<String> {
    let Some(path) = std::env::var_os("PATH") else {
        return Vec::new();
    };

    let mut found: Vec<String> = Vec::new();
    for dir in std::env::split_paths(&path) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            // Bound the scan: a pathological directory (a package cache, a
            // tensor blob dir) must not stall startup.
            if found.len() >= SCAN_CAP {
                break;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if !meta.is_file() || !is_executable(&meta) {
                continue;
            }
            if let Some(name) = entry.file_name().to_str() {
                if !name.starts_with('.') {
                    found.push(name.to_string());
                }
            }
        }
    }

    found.sort_unstable();
    found.dedup();
    found.sort_by_key(|name| (name.chars().count(), name.clone()));
    found.truncate(PATH_WORD_CAP);
    found
}

#[cfg(unix)]
fn is_executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_meta: &std::fs::Metadata) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(list: &[&str]) -> Vec<String> {
        list.iter().map(|word| word.to_string()).collect()
    }

    fn slash_entries(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(name, description)| (name.to_string(), description.to_string()))
            .collect()
    }

    /// Names of the candidates, in the order the menu would show them.
    fn names(candidates: &[Candidate]) -> Vec<&str> {
        candidates
            .iter()
            .map(|candidate| candidate.display.as_str())
            .collect()
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

    #[test]
    fn slash_source_lists_all_when_prefix_is_slash() {
        let source = SlashCommandSource::new(slash_entries(&[
            ("/help", "Show help"),
            ("/model", "Switch model"),
            ("/diagnose", "Diagnose"),
        ]));
        let candidates = source.complete("/", 1);
        assert_eq!(names(&candidates), ["/help", "/model", "/diagnose"]);
        assert_eq!(candidates.len(), source.len());
    }

    #[test]
    fn slash_source_filters_by_prefix() {
        let source = SlashCommandSource::new(slash_entries(&[
            ("/help", "Show help"),
            ("/model", "Switch model"),
            ("/mcp", "MCP servers"),
            ("/memory", "Memories"),
            ("/status", "Status"),
        ]));
        assert_eq!(
            names(&source.complete("/m", 2)),
            ["/model", "/mcp", "/memory"]
        );
        assert_eq!(names(&source.complete("/mo", 3)), ["/model"]);
        // A complete command stays a candidate, so Tab never drops it.
        assert_eq!(names(&source.complete("/status", 7)), ["/status"]);
        assert!(source.complete("/zzz", 4).is_empty());
    }

    #[test]
    fn slash_source_ignores_non_slash_token() {
        let source = SlashCommandSource::new(slash_entries(&[
            ("/help", "Show help"),
            ("/model", "Switch model"),
        ]));
        // Empty token, a bare word and a later argument are not slash commands.
        assert!(source.complete("", 0).is_empty());
        assert!(source.complete("he", 2).is_empty());
        assert!(source.complete("/help arg", 9).is_empty());
        // A `/`-prefixed token is completed wherever it sits on the line.
        assert_eq!(names(&source.complete("git /m", 6)), ["/model"]);
        assert_eq!(names(&source.complete("aish /m", 7)), ["/model"]);
    }

    #[test]
    fn slash_source_has_descriptions() {
        let source = SlashCommandSource::new(slash_entries(&[
            ("help", "Show help"),
            ("/model", "Switch model"),
            ("/bare", ""),
        ]));
        let candidates = source.complete("/", 1);
        // Missing leading slash is normalized, empty descriptions are dropped.
        assert_eq!(names(&candidates), ["/help", "/model", "/bare"]);
        assert_eq!(candidates[0].description.as_deref(), Some("Show help"));
        assert_eq!(candidates[1].description.as_deref(), Some("Switch model"));
        assert_eq!(candidates[2].description, None);
        // The description never leaks into what Tab inserts.
        assert_eq!(candidates[1].replacement, "/model");
    }

    #[test]
    fn slash_source_dedupes_names() {
        let source =
            SlashCommandSource::new(slash_entries(&[("/model", "First"), ("model", "Second")]));
        let candidates = source.complete("/", 1);
        assert_eq!(names(&candidates), ["/model"]);
        assert_eq!(candidates[0].description.as_deref(), Some("First"));
    }
}
