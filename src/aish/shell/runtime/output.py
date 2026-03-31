"""PTY output processing for the shell runtime."""

from __future__ import annotations

import sys
from typing import TYPE_CHECKING, Optional

from ...i18n import t

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

    def set_waiting_for_result(self, waiting: bool, command: str = "") -> None:
        """Set whether we're waiting for a command result."""
        self._waiting_for_result = waiting
        if waiting:
            self._current_command = command
            self.pty_manager.exit_tracker.clear_exit_available()

    def set_current_command(self, command: str) -> None:
        """Set the current command being executed."""
        self._current_command = command

    def set_filter_exit_echo(self, filter_exit: bool) -> None:
        """Set whether to filter exit command echo."""
        self._filter_exit_echo = filter_exit

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
                b"m ",
            )
            for pattern in prompt_patterns:
                if data.endswith(pattern):
                    placeholder_seq = self.placeholder_manager.show_placeholder()
                    if placeholder_seq:
                        data = data + placeholder_seq
                    break

        tracker = self.pty_manager.exit_tracker

        # Check for exit code marker regardless of _waiting_for_result.
        # This handles commands from bash readline (up-arrow, etc.) that bypass the router.
        if tracker.has_exit_code():
            # Add shell history to context when tracking a command
            if self._waiting_for_result and self.shell and self._current_command:
                self.shell.add_shell_history(
                    command=self._current_command,
                    returncode=tracker.last_exit_code,
                    stdout="",
                    stderr="",
                    offload={"status": "inline", "source": "pty"},
                )

            error_info = tracker.consume_error()
            if error_info is not None:
                hint = t("shell.error_correction.press_semicolon_hint")
                sys.stdout.write(f"\033[33m<{hint}>\033[0m\r\n")
                sys.stdout.flush()
            self._waiting_for_result = False
            self._current_command = ""
            tracker.clear_exit_available()

        return data
