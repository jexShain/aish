"""Tests for TUI adapters."""


from aish.tui.adapters.pty_adapter import (
    PTYOutputAdapter,
    extract_last_executable_command,
    is_interactive_command,
    INTERACTIVE_COMMANDS,
)
from aish.tui.types import PTYMode


class TestExtractLastExecutableCommand:
    """Test cases for extract_last_executable_command."""

    def test_simple_command(self):
        """Test simple command extraction."""
        assert extract_last_executable_command("vim") == "vim"
        assert extract_last_executable_command("ls -la") == "ls"

    def test_piped_command(self):
        """Test piped command extraction."""
        assert extract_last_executable_command("cat file | less") == "less"
        assert extract_last_executable_command("ls -la | grep test | less") == "less"

    def test_sequential_command(self):
        """Test sequential command extraction."""
        assert extract_last_executable_command("ls ; more file") == "more"
        assert extract_last_executable_command("cd / && pwd") == "pwd"

    def test_logical_command(self):
        """Test logical command extraction."""
        assert extract_last_executable_command("true || echo x") == "echo"
        assert extract_last_executable_command("ls && cat file | more") == "more"

    def test_sudo_command(self):
        """Test sudo command extraction."""
        assert extract_last_executable_command("sudo vim /etc/hosts") == "vim"
        assert extract_last_executable_command("sudo -u user cat file") == "cat"

    def test_empty_command(self):
        """Test empty command."""
        assert extract_last_executable_command("") == ""
        assert extract_last_executable_command("   ") == ""

    def test_path_command(self):
        """Test command with path."""
        assert extract_last_executable_command("/usr/bin/vim file") == "vim"
        assert extract_last_executable_command("./script.sh") == "script.sh"


class TestIsInteractiveCommand:
    """Test cases for is_interactive_command."""

    def test_interactive_editors(self):
        """Test interactive editor detection."""
        assert is_interactive_command("vim file.txt") is True
        assert is_interactive_command("vi file.txt") is True
        assert is_interactive_command("nano file.txt") is True
        assert is_interactive_command("emacs file.txt") is True

    def test_interactive_pagers(self):
        """Test interactive pager detection."""
        assert is_interactive_command("less file.txt") is True
        assert is_interactive_command("more file.txt") is True

    def test_interactive_system(self):
        """Test interactive system tools."""
        assert is_interactive_command("top") is True
        assert is_interactive_command("htop") is True
        assert is_interactive_command("btop") is True

    def test_interactive_network(self):
        """Test interactive network tools."""
        assert is_interactive_command("ssh user@host") is True
        assert is_interactive_command("telnet localhost 80") is True

    def test_interactive_shells(self):
        """Test interactive shell detection."""
        assert is_interactive_command("python") is True
        assert is_interactive_command("ipython") is True
        assert is_interactive_command("bash") is True
        assert is_interactive_command("zsh") is True

    def test_non_interactive_commands(self):
        """Test non-interactive commands."""
        assert is_interactive_command("ls -la") is False
        assert is_interactive_command("cat file.txt") is False
        assert is_interactive_command("echo hello") is False
        assert is_interactive_command("grep pattern file") is False

    def test_compound_commands(self):
        """Test compound command detection."""
        assert is_interactive_command("ls ; vim file") is True
        assert is_interactive_command("cat file | less") is True


class TestPTYOutputAdapter:
    """Test cases for PTYOutputAdapter."""

    def test_init(self, tui_app):
        """Test adapter initialization."""
        adapter = PTYOutputAdapter(tui_app)
        assert adapter.tui_app == tui_app

    def test_should_use_passthrough_interactive(self, tui_app):
        """Test passthrough detection for interactive commands."""
        adapter = PTYOutputAdapter(tui_app)

        assert adapter.should_use_passthrough("vim file.txt") is True
        assert adapter.should_use_passthrough("less file.txt") is True
        assert adapter.should_use_passthrough("top") is True

    def test_should_use_passthrough_non_interactive(self, tui_app):
        """Test passthrough detection for non-interactive commands."""
        adapter = PTYOutputAdapter(tui_app)

        assert adapter.should_use_passthrough("ls -la") is False
        assert adapter.should_use_passthrough("cat file.txt") is False
        assert adapter.should_use_passthrough("echo hello") is False

    def test_capture_context_manager(self, tui_app):
        """Test capture context manager."""
        adapter = PTYOutputAdapter(tui_app)

        with adapter.capture():
            assert tui_app.pty_mode == PTYMode.CAPTURE

    def test_process_output_capture_mode(self, tui_app):
        """Test output processing in capture mode."""
        tui_app.set_pty_mode(PTYMode.CAPTURE)
        adapter = PTYOutputAdapter(tui_app)

        initial_count = len(tui_app.state.content_lines)
        adapter.process_output("test output")

        assert len(tui_app.state.content_lines) > initial_count


class TestInteractiveCommandsSet:
    """Test the INTERACTIVE_COMMANDS set."""

    def test_contains_common_editors(self):
        """Test that common editors are in the set."""
        assert "vim" in INTERACTIVE_COMMANDS
        assert "vi" in INTERACTIVE_COMMANDS
        assert "nano" in INTERACTIVE_COMMANDS
        assert "emacs" in INTERACTIVE_COMMANDS

    def test_contains_common_pagers(self):
        """Test that common pagers are in the set."""
        assert "less" in INTERACTIVE_COMMANDS
        assert "more" in INTERACTIVE_COMMANDS

    def test_contains_system_monitors(self):
        """Test that system monitors are in the set."""
        assert "top" in INTERACTIVE_COMMANDS
        assert "htop" in INTERACTIVE_COMMANDS
        assert "btop" in INTERACTIVE_COMMANDS

    def test_contains_shells(self):
        """Test that shells are in the set."""
        assert "bash" in INTERACTIVE_COMMANDS
        assert "zsh" in INTERACTIVE_COMMANDS
        assert "fish" in INTERACTIVE_COMMANDS
        assert "python" in INTERACTIVE_COMMANDS
