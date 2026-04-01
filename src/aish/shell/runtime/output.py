"""PTY output processing for the shell runtime."""

from __future__ import annotations

import sys
from typing import TYPE_CHECKING, Optional

from ...i18n import t
from ...pty.control_protocol import BackendControlEvent

if TYPE_CHECKING:
    from ...pty import PTYManager
    from ..ui.placeholder import PlaceholderManager
    from .app import PTYAIShell


class OutputProcessor:
    """Process PTY output. detect errors. show hints."""

    def __init__(
        self,
        pty_manager: "PTYManager",
        placeholder_manager: Optional["PlaceholderManager"] = None,
        shell: Optional["PTYAIShell"] = None,
    ):
        self.pty_manager = pty_manager
        self._waiting_for_result = False
        self._filter_exit_echo = False
        self.placeholder_manager = placeholder_manager
        self.shell = shell
        self._current_command: str = ""
        self._last_recorded_result: tuple[str, int] | None = None
        # Two-layer error suppression:
        # Layer 1 (here): _suppress_error_hint — UI-layer skip for one cycle
        #   (e.g., after Ctrl+C for exit, suppress the spurious hint).
        # Layer 2 (ExitCodeTracker): _suppress_error / _error_hint_shown —
        #   distinguishes user-initiated vs backend/AI commands and prevents
        #   repeated hints on prompt redraws.  See exit_tracker.py for details.
        self._suppress_error_hint: bool = False

    def set_waiting_for_result(self, waiting: bool, command: str = "") -> None:
        """Set whether we're waiting for a command result."""
        self._waiting_for_result = waiting
        self._suppress_error_hint = False
        if waiting:
            self._current_command = command
            self.pty_manager.exit_tracker.clear_exit_available()

    def suppress_next_error_hint(self) -> None:
        """Suppress the next error correction hint (e.g., after Ctrl+C for exit)."""
        self._suppress_error_hint = True

    def set_current_command(self, command: str) -> None:
        """Set the current command being executed."""
        self._current_command = command

    def set_filter_exit_echo(self, filter_exit: bool) -> None:
        """Set whether to filter exit command echo."""
        self._filter_exit_echo = filter_exit

    def handle_backend_event(self, event: BackendControlEvent) -> None:
        """Update output state from explicit backend lifecycle events."""
        if event.type != "prompt_ready":
            return

        self._waiting_for_result = False

    def process(self, data: bytes) -> bytes:
        """Process PTY output, return cleaned output."""
        if self._filter_exit_echo:
            stripped = data.strip(b"\r\n")
            if stripped == b"exit":
                return b""
            for pattern in (b"\rexit\r\n", b"\nexit\r\n", b"\rexit\n"):
                if data.endswith(pattern):
                    data = data[: -len(pattern)]
                    self._filter_exit_echo = False
                    break

        if self.placeholder_manager and not self._waiting_for_result:
            prompt_patterns = (
                b"$ ",
                b"# ",
                b"% ",
                b"> ",
                b"\x1b[0m ",
            )
            for pattern in prompt_patterns:
                if data.endswith(pattern):
                    placeholder_seq = self.placeholder_manager.show_placeholder()
                    if placeholder_seq:
                        data = data + placeholder_seq
                    break

        tracker = self.pty_manager.exit_tracker

        if tracker.has_exit_code():
            command = self._current_command or tracker.last_command
            result_key = (command, tracker.last_exit_code)
            if self.shell and command and self._last_recorded_result != result_key:
                self.shell.add_shell_history(
                    command=command,
                    returncode=tracker.last_exit_code,
                    stdout="",
                    stderr="",
                    offload={"status": "inline", "source": "pty"},
                )
                self._last_recorded_result = result_key

            error_info = tracker.consume_error()
            if error_info is not None:
                if self._suppress_error_hint:
                    self._suppress_error_hint = False
                else:
                    hint = t("shell.error_correction.press_semicolon_hint")
                    sys.stdout.write(f"\033[2m\033[37m<{hint}>\033[0m\r\n")
                    sys.stdout.flush()
            self._waiting_for_result = False
            self._current_command = ""
            tracker.clear_exit_available()

        return data
