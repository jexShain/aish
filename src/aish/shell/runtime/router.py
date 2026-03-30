"""Input routing for the PTY shell runtime."""

from __future__ import annotations

import sys
import threading
from typing import TYPE_CHECKING, Optional

from wcwidth import wcwidth

from ...interruption import InterruptAction
from ..ui.suggestions import SuggestionEngine

if TYPE_CHECKING:
    from ...interruption import InterruptionManager
    from ...pty import PTYManager
    from .ai import AIHandler
    from .output import OutputProcessor
    from ..ui.placeholder import PlaceholderManager


class InputRouter:
    """Route user input to PTY or AI handler.

    Detects ';' at line start to trigger AI mode.
    """

    SEMICOLON_MARKS = frozenset({";", "；"})

    INVISIBLE_CHARS = frozenset(
        {
            "\u200b",
            "\u200c",
            "\u200d",
            "\u200e",
            "\u200f",
            "\u061c",
            "\ufeff",
            "\u00ad",
            "\u180e",
            "\u2060",
            "\u2061",
            "\u2062",
            "\u2063",
            "\u2064",
            "\u2066",
            "\u2067",
            "\u2068",
            "\u2069",
            "\u206a",
            "\u206b",
            "\u206c",
            "\u206d",
            "\u206e",
            "\u206f",
            "\u034f",
            "\u17b4",
            "\u17b5",
        }
    )

    ESC = "\x1b"

    def __init__(
        self,
        pty_manager: "PTYManager",
        ai_handler: "AIHandler",
        output_processor: Optional["OutputProcessor"] = None,
        placeholder_manager: Optional["PlaceholderManager"] = None,
        interruption_manager: Optional["InterruptionManager"] = None,
        history_manager=None,
    ):
        self.pty_manager = pty_manager
        self.ai_handler = ai_handler
        self.output_processor = output_processor
        self.placeholder_manager = placeholder_manager
        self.interruption_manager = interruption_manager
        self._buffer = ""
        self._at_line_start = True
        self._in_ai_mode = False
        self._ai_buffer = ""
        self._current_cmd = ""
        self._in_bracketed_paste = False
        self._paste_buffer = b""
        self._placeholder_cleared = False
        self._placeholder_refresh_timer: Optional[threading.Timer] = None
        self._suggestion_engine = SuggestionEngine(history_manager=history_manager)

    def handle_input(self, data: bytes) -> None:
        """Process input bytes and route to PTY or AI."""
        if self._in_bracketed_paste:
            if b"\x1b[201~" in data:
                parts = data.split(b"\x1b[201~", 1)
                self._paste_buffer += parts[0]
                self._in_bracketed_paste = False
                pasted = self._paste_buffer
                self._paste_buffer = b""
                if pasted:
                    self._process_normal_input(pasted)
                if parts[1]:
                    self.handle_input(parts[1])
                return
            self._paste_buffer += data
            return

        if data.startswith(b"\x1b[200~"):
            self._in_bracketed_paste = True
            self._paste_buffer = b""
            remaining = data[6:]
            if remaining:
                self.handle_input(remaining)
            return

        if len(data) > 0 and data[0] == 0x1B:
            if not self._in_ai_mode and data == b"\x1b[C":
                suffix = self._suggestion_engine.accept()
                if suffix:
                    self.pty_manager.send(suffix.encode("utf-8"))
                    self._current_cmd += suffix
                    return
            if self._in_ai_mode:
                return
            self.pty_manager.send(data)
            return

        self._process_normal_input(data)

    def _process_normal_input(self, data: bytes) -> None:
        """Process normal (non-escape) input."""
        try:
            text = data.decode("utf-8", errors="replace")
        except Exception:
            self.pty_manager.send(data)
            return

        for char in text:
            self._handle_char(char)

    def _handle_char(self, char: str) -> None:
        """Handle a single character."""
        skip_clear_chars = {
            "\x01",
            "\x03",
            "\x04",
            "\x05",
            "\x07",
            "\x08",
            "\x09",
            "\x0b",
            "\x0c",
            "\x0e",
            "\x0f",
            "\x10",
            "\x1b",
            "\x7f",
        }

        if (
            not self._placeholder_cleared
            and self.placeholder_manager
            and char not in skip_clear_chars
        ):
            self._clear_placeholder()

        if char in ("\n", "\r"):
            if self._in_ai_mode:
                self._suggestion_engine.clear()
                self._process_ai_input()
                self._in_ai_mode = False
                self._ai_buffer = ""
                self._at_line_start = True
            else:
                self._suggestion_engine.clear()
                if self._current_cmd.strip():
                    cmd_stripped = self._current_cmd.strip()
                    self.pty_manager.exit_tracker.set_last_command(cmd_stripped)
                    is_exit_cmd = cmd_stripped in (
                        "exit",
                        "logout",
                    ) or cmd_stripped.startswith(("exit ", "logout "))
                    if self.output_processor:
                        if not is_exit_cmd:
                            self.output_processor.set_waiting_for_result(True)
                        else:
                            self.output_processor.set_filter_exit_echo(True)
                self.pty_manager.send(b"\r")
                self._at_line_start = True
                self._current_cmd = ""

            if self.placeholder_manager:
                self.placeholder_manager.reset_for_new_line()
            self._placeholder_cleared = False
            return

        if char == "\x03":
            self._suggestion_engine.clear()
            if self._in_ai_mode:
                self._in_ai_mode = False
                self._ai_buffer = ""
                self._at_line_start = True
                sys.stdout.write("\r\n^C\r\n")
                sys.stdout.flush()
                return
            if self.interruption_manager:
                has_input = bool(self._current_cmd.strip())
                action = self.interruption_manager.handle_ctrl_c(has_input)

                if self.placeholder_manager and self.placeholder_manager.is_visible():
                    clear_seq = self.placeholder_manager.clear_placeholder()
                    sys.stdout.buffer.write(clear_seq)
                    sys.stdout.buffer.flush()

                self.pty_manager.send(char.encode())
                self._current_cmd = ""

                if action == InterruptAction.CONFIRM_EXIT:
                    if self._placeholder_refresh_timer:
                        self._placeholder_refresh_timer.cancel()
                        self._placeholder_refresh_timer = None
                    shell = self.ai_handler.shell if self.ai_handler else None
                    if shell is not None:
                        shell._running = False
                elif action == InterruptAction.REQUEST_EXIT:
                    if self.interruption_manager:
                        self._schedule_placeholder_refresh(
                            self.interruption_manager.EXIT_WINDOW
                        )
                return

            self.pty_manager.send(char.encode())
            self._current_cmd = ""
            return

        if char in ("\x7f", "\x08"):
            if self._in_ai_mode:
                if self._ai_buffer:
                    last_char = self._ai_buffer[-1]
                    self._ai_buffer = self._ai_buffer[:-1]
                    char_width = wcwidth(last_char)
                    if char_width < 1:
                        char_width = 1
                    sys.stdout.write("\b \b" * char_width)
                    sys.stdout.flush()
                    self._suggestion_engine.update(self._ai_buffer, ai_mode=True)
                else:
                    semicolon_width = getattr(self, "_semicolon_width", 1)
                    sys.stdout.write("\b \b" * semicolon_width)
                    sys.stdout.flush()
                    self._suggestion_engine.clear()
                    self._in_ai_mode = False
                    self._at_line_start = True
                return

            self.pty_manager.send(char.encode())
            if self._current_cmd:
                self._current_cmd = self._current_cmd[:-1]
                if not self._current_cmd:
                    self._at_line_start = True
                self._suggestion_engine.update(self._current_cmd)
            return

        if self._at_line_start:
            if char in self.INVISIBLE_CHARS:
                return
            if char in self.SEMICOLON_MARKS:
                self._in_ai_mode = True
                self._ai_buffer = ""
                self._at_line_start = False
                self._semicolon_width = wcwidth(char)
                if self._semicolon_width < 1:
                    self._semicolon_width = 1
                sys.stdout.write(char)
                sys.stdout.flush()
                return
            if char == self.ESC:
                self.pty_manager.send(char.encode())
                return
            if char == "\x01":
                self.pty_manager.send(char.encode())
                return

        if self._in_ai_mode:
            self._ai_buffer += char
            sys.stdout.write(char)
            sys.stdout.flush()
            self._suggestion_engine.update(self._ai_buffer, ai_mode=True)
            return

        self.pty_manager.send(char.encode())
        self._current_cmd += char
        self._at_line_start = False
        self._suggestion_engine.update(self._current_cmd)

    def _process_ai_input(self) -> None:
        """Process collected AI input."""
        if not self._ai_buffer.strip():
            self.ai_handler.handle_error_correction()
        else:
            self.ai_handler.handle_question(self._ai_buffer)

    def _clear_placeholder(self) -> None:
        """Clear placeholder if visible."""
        if self.placeholder_manager and self.placeholder_manager.is_visible():
            clear_seq = self.placeholder_manager.clear_placeholder()
            sys.stdout.buffer.write(clear_seq)
            sys.stdout.buffer.flush()
            self.placeholder_manager.mark_cleared()
            self._placeholder_cleared = True

    def _schedule_placeholder_refresh(self, delay_seconds: float) -> None:
        """Schedule a placeholder refresh after Ctrl+C timeout."""
        if self._placeholder_refresh_timer:
            self._placeholder_refresh_timer.cancel()

        self._placeholder_refresh_timer = threading.Timer(
            delay_seconds, self._refresh_placeholder_after_timeout
        )
        self._placeholder_refresh_timer.daemon = True
        self._placeholder_refresh_timer.start()

    def _refresh_placeholder_after_timeout(self) -> None:
        """Refresh placeholder after timeout without newline."""
        if not self.interruption_manager or not self.placeholder_manager:
            self._placeholder_refresh_timer = None
            return

        from ...interruption import ShellState

        if self.interruption_manager.state != ShellState.NORMAL:
            self.interruption_manager.set_state(ShellState.NORMAL)
            self.interruption_manager.clear_prompt()

        try:
            if self.placeholder_manager.is_visible():
                clear_seq = self.placeholder_manager.clear_placeholder()
                sys.stdout.buffer.write(clear_seq)
                sys.stdout.buffer.flush()
        except Exception:
            pass

        try:
            show_seq = self.placeholder_manager.show_placeholder()
            if show_seq:
                sys.stdout.buffer.write(show_seq)
                sys.stdout.buffer.flush()
        except Exception:
            pass

        self._placeholder_refresh_timer = None