"""Exit code tracking via PROMPT_COMMAND marker."""

import re
from typing import Optional, Tuple


class ExitCodeTracker:
    """Track bash command exit codes using PROMPT_COMMAND marker.

    The marker format is: [AISH_EXIT:N] or [AISH_EXIT:N:command].
    This is injected via PROMPT_COMMAND in bash initialization.

    Error correction hints are ONLY shown for user-initiated commands:
    - Explicitly typed commands (router calls set_last_command)
    - Arrow-key history selections (detected via different command text in marker)
    Backend/AI commands never trigger hints.
    """

    # Format: [AISH_EXIT:code] or [AISH_EXIT:code:command]
    MARKER_PATTERN = re.compile(rb"\[AISH_EXIT:(-?\d+)(?::([^\]]+))?\]")

    def __init__(self):
        self._last_exit_code: int = 0
        self._last_command: str = ""
        self._has_error: bool = False
        self._exit_code_available: bool = False
        # Prevents repeated error hints on prompt redraws.
        # Set True after consume_error() returns info; reset when a new
        # user-initiated command is detected.
        self._error_hint_shown: bool = False
        # Set True by set_last_command() before execution; cleared after
        # marker processing.  Distinguishes a genuine new user-typed command
        # from prompt redraws.
        self._command_initiated: bool = False
        # When True, this is a backend/AI command — never trigger hints.
        self._suppress_error: bool = False

    @property
    def last_exit_code(self) -> int:
        """Get the last command's exit code."""
        return self._last_exit_code

    @property
    def has_error(self) -> bool:
        """Check if last command had non-zero exit code."""
        return self._has_error

    @property
    def last_command(self) -> str:
        """Get the last executed command."""
        return self._last_command

    def set_last_command(self, command: str) -> None:
        """Set the command that's about to be executed (user-initiated).

        Resets error hint state so the next failure can trigger a hint.
        """
        self._last_command = command
        self._error_hint_shown = False
        self._command_initiated = True

    def set_backend_command(self, command: str) -> None:
        """Set command from a backend/AI source.

        Records the command for exit code tracking but does NOT
        reset error hint state — AI tool failures should not trigger
        error correction hints.
        """
        self._last_command = command
        self._suppress_error = True

    def parse_and_update(self, data: bytes) -> bytes:
        """Parse exit code marker from PTY output, update state, return cleaned output."""
        markers = list(self.MARKER_PATTERN.finditer(data))

        if markers:
            last_marker = markers[-1]
            exit_code = int(last_marker.group(1))
            self._last_exit_code = exit_code
            self._exit_code_available = True

            # Detect whether this marker represents a new user-initiated
            # command, before updating _last_command.
            is_new_user_command = self._command_initiated

            command_in_marker = last_marker.group(2)
            if command_in_marker:
                # Decode %5D back to ] (encoded in bash_rc_wrapper.sh)
                decoded = command_in_marker.decode(
                    "utf-8", errors="replace"
                ).replace("%5D", "]")
                if decoded:
                    # A different command text in the marker means a new
                    # command was executed (e.g., arrow-key history selection
                    # that bypasses the router).
                    if decoded != self._last_command:
                        is_new_user_command = True
                    if is_new_user_command:
                        self._error_hint_shown = False
                    self._last_command = decoded

            # Only set _has_error for user-initiated commands that haven't
            # been hinted yet.  Backend commands and prompt redraws are
            # suppressed.  NOTE: the `and` chain order matters —
            # `_suppress_error` is checked before `is_new_user_command` so
            # that a backend command whose marker text differs from
            # _last_command (setting is_new_user_command=True) is still
            # correctly suppressed.
            if exit_code != 0:
                if (
                    not self._error_hint_shown
                    and not self._suppress_error
                    and is_new_user_command
                ):
                    self._has_error = True
                else:
                    self._has_error = False
                    # Backend commands: mark hint as "shown" so redraws
                    # don't accidentally trigger it later.
                    if self._suppress_error:
                        self._error_hint_shown = True
            else:
                # Success clears error state
                self._has_error = False
                self._error_hint_shown = False

            self._command_initiated = False
            self._suppress_error = False

            cleaned = self.MARKER_PATTERN.sub(b"", data)
            return cleaned
        return data

    def consume_error(self) -> Optional[Tuple[str, int]]:
        """Consume and return error info. Auto-resets has_error and marks hint as shown."""
        if self._has_error:
            cmd = self._last_command
            code = self._last_exit_code
            self._has_error = False
            self._error_hint_shown = True
            return cmd, code
        return None

    def consume_exit_code(self) -> Optional[Tuple[str, int]]:
        """Consume and return exit code info if a command completed."""
        if self._exit_code_available:
            cmd = self._last_command
            code = self._last_exit_code
            self._exit_code_available = False
            return cmd, code
        return None

    def clear_exit_available(self) -> None:
        """Clear exit code available flag."""
        self._exit_code_available = False

    def clear_error(self) -> None:
        """Clear the error state."""
        self._has_error = False
        self._error_hint_shown = False

    def clear_error_correction(self) -> None:
        """Clear error state so subsequent prompt redraws do not re-trigger
        the correction hint.

        Called when the user presses Enter on empty line.  Keeps
        _last_command intact so that the next PROMPT_COMMAND marker
        (which still contains the same command from bash history) is
        NOT mis-detected as a new user command.
        """
        self._has_error = False
        self._error_hint_shown = True

    def mark_backend_error_suppressed(self) -> None:
        """Mark that a backend command's error has been suppressed.

        Ensures subsequent prompt redraws will NOT show the error
        correction hint. Called after execute_command() completes
        for AI/backend tool execution.
        """
        self._has_error = False
        self._error_hint_shown = True
        self._suppress_error = False

    def has_exit_code(self) -> bool:
        """Check if exit code is available."""
        return self._exit_code_available

    def reset(self) -> None:
        """Reset all state."""
        self._last_exit_code = 0
        self._last_command = ""
        self._has_error = False
        self._exit_code_available = False
        self._error_hint_shown = False
        self._command_initiated = False
        self._suppress_error = False
