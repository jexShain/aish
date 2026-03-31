"""Exit code tracking via PROMPT_COMMAND marker."""

import re
from typing import Optional, Tuple


class ExitCodeTracker:
    """Track bash command exit codes using PROMPT_COMMAND marker.

    The marker format is: [AISH_EXIT:N] or [AISH_EXIT:N:command].
    This is injected via PROMPT_COMMAND in bash initialization.
    """

    # Format: [AISH_EXIT:code] or [AISH_EXIT:code:command]
    MARKER_PATTERN = re.compile(rb"\[AISH_EXIT:(-?\d+)(?::([^\]]+))?\]")

    def __init__(self):
        self._last_exit_code: int = 0
        self._last_command: str = ""
        self._has_error: bool = False
        self._exit_code_available: bool = False
        # Prevents repeated error hints on prompt redraws.
        # Set True after consume_error() returns info; reset when a new command is detected.
        self._error_hint_shown: bool = False

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
        """Set the command that's about to be executed."""
        self._last_command = command
        self._error_hint_shown = False

    def parse_and_update(self, data: bytes) -> bytes:
        """Parse exit code marker from PTY output, update state, return cleaned output."""
        markers = list(self.MARKER_PATTERN.finditer(data))

        if markers:
            last_marker = markers[-1]
            exit_code = int(last_marker.group(1))
            self._last_exit_code = exit_code
            self._exit_code_available = True

            # Extract command from marker if present
            command_in_marker = last_marker.group(2)
            if command_in_marker:
                # Decode %5D back to ] (encoded in bash_rc_wrapper.sh)
                decoded = command_in_marker.decode(
                    "utf-8", errors="replace"
                ).replace("%5D", "]")
                if decoded:
                    # New command detected via marker — reset hint flag
                    if decoded != self._last_command:
                        self._error_hint_shown = False
                    self._last_command = decoded

            # Only set _has_error if we haven't already shown the hint for this error.
            # This prevents repeated hints on prompt redraws where $? is unchanged.
            if exit_code != 0:
                if not self._error_hint_shown:
                    self._has_error = True
            else:
                # Success clears error state
                self._has_error = False
                self._error_hint_shown = False

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
