"""PTY output processing for the shell runtime."""

from __future__ import annotations

import sys
from typing import TYPE_CHECKING, Optional

from ...i18n import t

if TYPE_CHECKING:
    from ...pty import PTYManager
    from ..ui.placeholder import PlaceholderManager


class OutputProcessor:
    """Process PTY output, detect errors, show hints."""

    def __init__(
        self,
        pty_manager: "PTYManager",
        placeholder_manager: Optional["PlaceholderManager"] = None,
    ):
        self.pty_manager = pty_manager
        self._waiting_for_result = False
        self._filter_exit_echo = False
        self.placeholder_manager = placeholder_manager

    def set_waiting_for_result(self, waiting: bool) -> None:
        """Set whether we're waiting for a command result."""
        self._waiting_for_result = waiting
        if waiting:
            self.pty_manager.exit_tracker.clear_exit_available()

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

        if not self._waiting_for_result:
            return data

        tracker = self.pty_manager.exit_tracker
        if tracker.has_exit_code():
            error_info = tracker.consume_error()
            if error_info is not None:
                hint = t("shell.error_correction.press_semicolon_hint")
                sys.stdout.write(f"\033[33m<{hint}>\033[0m\r\n")
                sys.stdout.flush()
            self._waiting_for_result = False
            tracker.clear_exit_available()

        return data