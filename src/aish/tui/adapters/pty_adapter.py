"""PTY output adapter for TUI."""

import sys
from contextlib import contextmanager
from typing import TYPE_CHECKING, Generator, Optional, Set

from aish.tui.types import PTYMode

if TYPE_CHECKING:
    from aish.tui.app import TUIApp


# Set of interactive commands that require passthrough mode
INTERACTIVE_COMMANDS: Set[str] = {
    "vim",
    "vi",
    "nano",
    "emacs",
    "less",
    "more",
    "top",
    "htop",
    "btop",
    "ssh",
    "telnet",
    "tmux",
    "screen",
    "su",
    "sudo",
    "python",
    "ipython",
    "gdb",
    "man",
    "info",
    "psql",
    "mysql",
    "sqlite3",
    "redis-cli",
    "mongosh",
    "nu",
    "fish",
    "zsh",
    "bash",
    "sh",
}


def extract_last_executable_command(command: str) -> str:
    """Extract the last executable command from a potentially complex command string.

    Handles pipes, &&, ||, and other shell constructs.

    Args:
        command: Full command string

    Returns:
        The last executable command name
    """
    # Remove leading/trailing whitespace
    command = command.strip()

    # Handle common shell operators
    for operator in ["&&", "||", "|", ";"]:
        if operator in command:
            # Split and take the last part
            parts = command.split(operator)
            command = parts[-1].strip()

    # Extract command name (first word)
    if not command:
        return ""

    # Handle sudo/su - extract the actual command
    parts = command.split()
    while parts and parts[0] in ("sudo", "su"):
        parts = parts[1:]
        # Skip -u username, -c "command", etc.
        while parts and parts[0].startswith("-"):
            if parts[0] in ("-c", "-u"):
                parts = parts[1:]  # Skip flag value
            parts = parts[1:]

    if not parts:
        return ""

    # Get command name, remove path if present
    cmd_name = parts[0]
    if "/" in cmd_name:
        cmd_name = cmd_name.split("/")[-1]

    return cmd_name


def is_interactive_command(command: str) -> bool:
    """Check if a command requires interactive terminal access.

    Args:
        command: Command string to check

    Returns:
        True if command is interactive
    """
    cmd_name = extract_last_executable_command(command)

    # Check against known interactive commands
    if cmd_name in INTERACTIVE_COMMANDS:
        return True

    # Check for common patterns
    # Commands starting with specific prefixes
    interactive_prefixes = ("vim", "nano", "emacs", "less", "more", "top", "htop")
    for prefix in interactive_prefixes:
        if cmd_name.startswith(prefix):
            return True

    return False


class PTYOutputAdapter:
    """Adapter for capturing or passing through PTY output in TUI mode."""

    def __init__(self, tui_app: "TUIApp"):
        """Initialize PTY adapter.

        Args:
            tui_app: Reference to TUIApp instance
        """
        self.tui_app = tui_app
        self._original_stdout: Optional[int] = None
        self._original_stderr: Optional[int] = None
        self._capture_buffer = ""

    @contextmanager
    def capture(self) -> Generator[None, None, None]:
        """Context manager to capture PTY output to TUI content area.

        Use for non-interactive commands where output should be displayed
        in the TUI content area.

        Yields:
            None
        """
        self.tui_app.set_pty_mode(PTYMode.CAPTURE)
        try:
            yield
        finally:
            # Flush any remaining captured output
            if self._capture_buffer:
                self.tui_app.append_pty_output(self._capture_buffer)
                self._capture_buffer = ""

    @contextmanager
    def passthrough(self) -> Generator[None, None, None]:
        """Context manager for passthrough mode (interactive commands).

        Use for interactive commands that need direct terminal access.
        The TUI will be temporarily suspended.

        Yields:
            None
        """
        self.tui_app.set_pty_mode(PTYMode.PASSTHROUGH)

        # Save current terminal state
        try:
            import termios
            import tty

            # Get current terminal settings
            fd = sys.stdin.fileno()
            old_settings = termios.tcgetattr(fd)

            # Set raw mode for passthrough
            tty.setraw(fd)

            try:
                yield
            finally:
                # Restore terminal settings
                termios.tcsetattr(fd, termios.TCSADRAIN, old_settings)

        except (ImportError, termios.error, OSError):
            # On systems without termios (e.g., Windows), just yield
            yield

    def should_use_passthrough(self, command: str) -> bool:
        """Determine if a command should use passthrough mode.

        Args:
            command: Command string to check

        Returns:
            True if passthrough mode should be used
        """
        return is_interactive_command(command)

    def process_output(self, output: str) -> None:
        """Process PTY output based on current mode.

        In CAPTURE mode, output is added to TUI content area.
        In PASSTHROUGH mode, output is written directly to terminal.

        Args:
            output: PTY output string
        """
        if self.tui_app.pty_mode == PTYMode.PASSTHROUGH:
            # Write directly to terminal
            sys.stdout.write(output)
            sys.stdout.flush()
        else:
            # Buffer for capture
            self.tui_app.append_pty_output(output)
