"""Test error hint behavior for repeated commands and backend commands."""

import pytest

from aish.pty.exit_tracker import ExitCodeTracker


def _make_marker(exit_code: int, command: str) -> bytes:
    """Build an AISH_EXIT marker."""
    return f"[AISH_EXIT:{exit_code}:{command}]".encode()


@pytest.mark.timeout(5)
def test_same_command_failure_retriggers_after_consume():
    """Same command failing twice (with new execution) should show hint both times."""
    tracker = ExitCodeTracker()

    tracker.set_last_command("ls-a")
    tracker.parse_and_update(_make_marker(127, "ls-a"))
    assert tracker.has_error is True
    error1 = tracker.consume_error()
    assert error1 == ("ls-a", 127)

    tracker.set_last_command("ls-a")
    tracker.parse_and_update(_make_marker(127, "ls-a"))
    assert tracker.has_error is True
    error2 = tracker.consume_error()
    assert error2 == ("ls-a", 127)


@pytest.mark.timeout(5)
def test_prompt_redraw_no_duplicate_hint():
    """Prompt redraw (same marker, no set_last_command) must not re-trigger hint."""
    tracker = ExitCodeTracker()

    tracker.set_last_command("bad")
    tracker.parse_and_update(_make_marker(1, "bad"))
    assert tracker.consume_error() == ("bad", 1)

    tracker.parse_and_update(_make_marker(1, "bad"))
    assert tracker.has_error is False
    assert tracker.consume_error() is None


@pytest.mark.timeout(5)
def test_backend_command_no_error_hint():
    """Commands from bash_exec/AI tools should NOT trigger error hints."""
    tracker = ExitCodeTracker()

    tracker.set_backend_command("rm -rf /protected")
    tracker.parse_and_update(_make_marker(1, "rm -rf /protected"))
    assert tracker.has_error is False
    assert tracker.consume_error() is None


@pytest.mark.timeout(5)
def test_backend_error_prompt_redraw_no_hint():
    """After backend command fails, prompt redraws must NOT show error hint.

    Regression: _suppress_error was cleared after first marker processing,
    so prompt redraws re-triggered the hint via _error_hint_shown still being False.
    """
    tracker = ExitCodeTracker()

    # Backend command fails
    tracker.set_backend_command("ipaw")
    tracker.parse_and_update(_make_marker(127, "ipaw"))
    assert tracker.has_error is False

    # Prompt redraw sends same marker — must NOT show hint
    tracker.parse_and_update(_make_marker(127, "ipaw"))
    assert tracker.has_error is False
    assert tracker.consume_error() is None

    # Another redraw — still no hint
    tracker.parse_and_update(_make_marker(127, "ipaw"))
    assert tracker.has_error is False


@pytest.mark.timeout(5)
def test_backend_error_does_not_affect_next_user_command():
    """After a backend command fails, next user command failure should show hint."""
    tracker = ExitCodeTracker()

    tracker.set_backend_command("bad-ai-cmd")
    tracker.parse_and_update(_make_marker(1, "bad-ai-cmd"))
    assert tracker.has_error is False

    # User command fails — should show hint
    tracker.set_last_command("user-cmd")
    tracker.parse_and_update(_make_marker(1, "user-cmd"))
    assert tracker.has_error is True
    assert tracker.consume_error() == ("user-cmd", 1)


@pytest.mark.timeout(5)
def test_up_arrow_reexecution_shows_hint():
    """Up arrow + Enter: _command_initiated set directly should trigger hint."""
    tracker = ExitCodeTracker()

    tracker.set_last_command("ls-a")
    tracker.parse_and_update(_make_marker(127, "ls-a"))
    tracker.consume_error()

    tracker._command_initiated = True
    tracker.parse_and_update(_make_marker(127, "ls-a"))
    assert tracker.has_error is True
    assert tracker.consume_error() == ("ls-a", 127)


@pytest.mark.timeout(5)
def test_successful_command_clears_error():
    """A successful command after an error should clear the error state."""
    tracker = ExitCodeTracker()

    tracker.set_last_command("bad")
    tracker.parse_and_update(_make_marker(1, "bad"))
    tracker.consume_error()

    tracker.set_last_command("good")
    tracker.parse_and_update(_make_marker(0, "good"))
    assert tracker.has_error is False
    assert tracker.consume_error() is None


@pytest.mark.timeout(5)
def test_backend_suppress_cleared_after_marker():
    """_suppress_error is cleared after processing a marker."""
    tracker = ExitCodeTracker()

    tracker.set_backend_command("cmd1")
    tracker.parse_and_update(_make_marker(1, "cmd1"))
    assert tracker._suppress_error is False

    tracker.set_last_command("cmd2")
    tracker.parse_and_update(_make_marker(1, "cmd2"))
    assert tracker.has_error is True
