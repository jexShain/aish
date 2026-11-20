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
    clippy::too_many_arguments,
    clippy::useless_conversion
)]

pub mod ai_handler;
pub mod animation;
#[macro_use]
pub mod app;
pub mod autosuggest;
pub mod commands;
pub mod completion;
pub mod completion_list;
pub mod doctor;
pub mod environment;
pub mod esc_watcher;
pub mod expand_history;
pub(crate) mod failure_hint;
pub mod feedback;
pub mod image;
pub mod inline_completion;
pub mod input;
pub mod keyboard;
pub mod llm_event_ui;
pub mod md_render;
pub mod nl_detect;
pub mod prompt;
pub mod readline;
pub mod recorder;
pub mod renderer;
pub(crate) mod report;
pub mod resource_monitor;
pub mod resume_selector;
pub mod security_panel;
pub mod session_transcript;
pub mod settings_panel;
pub mod shell_session;
pub mod status;
pub mod theme;
pub mod token_store;
pub mod tui;
pub mod types;
pub mod update_check;
pub mod wizard;

pub use app::AishShell;
pub use types::{InputIntent, ShellState};

/// The built-in slash-command catalogue as `(name, description)` pairs, in
/// menu order.
///
/// This is the public face of [`readline::SLASH_COMMANDS`] for front ends that
/// cannot depend on this crate's internals: a terminal UI asks for it and
/// feeds the result to its own completion menu. Descriptions are translated
/// with the current locale, matching what the `aish_ui` popup renders.
pub fn slash_command_catalog() -> Vec<(String, String)> {
    readline::SLASH_COMMANDS
        .iter()
        .map(|command| {
            let name = command.name.trim_start_matches('/');
            let description = aish_i18n::t(&format!("shell.slash.{name}"));
            (command.name.to_string(), description)
        })
        .collect()
}

/// Check whether the configuration requires an interactive setup wizard.
///
/// Returns `true` when either the model or API key is missing from the
/// configuration (and no CLI override was provided for the missing field).
pub fn needs_interactive_setup(config: &aish_config::ConfigModel) -> bool {
    if config.model.trim().is_empty() {
        return true;
    }
    if config.api_key.trim().is_empty() {
        return true;
    }
    false
}

/// Serialize tests that mutate process-global environment variables.
///
/// Unit tests within this crate share one process and run concurrently, so any
/// test that repoints `HOME`, `XDG_*` or `AISH_*` must hold this lock: several
/// subsystems (`dirs::config_dir`, `dirs::data_local_dir`, policy discovery,
/// skill roots) read those variables lazily, and an interleaved write from a
/// sibling test would redirect them at a temporary directory.
#[cfg(test)]
pub(crate) fn env_test_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::LazyLock<std::sync::Mutex<()>> =
        std::sync::LazyLock::new(|| std::sync::Mutex::new(()));
    &LOCK
}

#[cfg(test)]
mod catalog_tests {
    use super::*;

    #[test]
    fn catalog_lists_every_slash_command_with_a_translation() {
        let catalog = slash_command_catalog();
        assert_eq!(catalog.len(), readline::SLASH_COMMANDS.len());
        for (name, description) in &catalog {
            assert!(name.starts_with('/'), "not a slash command: {name}");
            assert!(
                !description.is_empty() && !description.starts_with("shell.slash."),
                "untranslated description for {name}: {description}"
            );
        }
    }

    #[test]
    fn catalog_keeps_catalogue_order_and_the_commands_the_tui_menu_needs() {
        let catalog = slash_command_catalog();
        let names: Vec<&str> = catalog.iter().map(|(name, _)| name.as_str()).collect();
        // The TUI menu shows the first MENU_ROWS entries: the commands users
        // reach for must be inside that head.
        let head: Vec<&str> = names.iter().take(10).copied().collect();
        for expected in ["/diagnose", "/status", "/live_sessions", "/model"] {
            assert!(head.contains(&expected), "{expected} not in {head:?}");
        }
        assert_eq!(
            names.first().copied(),
            Some("/help"),
            "catalogue order drives the menu: {names:?}"
        );
    }
}
