"""Focused unit tests for the PTY shell core."""

from __future__ import annotations

from unittest.mock import Mock

from aish.i18n import t
from aish.shell.runtime.output import OutputProcessor
from aish.shell.runtime.router import InputRouter


class _FakeTracker:
    def __init__(self, *, has_exit_code: bool = False, error_info=None):
        self._has_exit_code = has_exit_code
        self._error_info = error_info
        self.last_exit_code = 0
        self.last_command = ""
        self.clear_exit_available = Mock()
        self.set_last_command = Mock(side_effect=self._remember_command)

    def _remember_command(self, command: str) -> None:
        self.last_command = command

    def has_exit_code(self) -> bool:
        return self._has_exit_code

    def consume_error(self):
        return self._error_info


class _FakePTYManager:
    def __init__(self, tracker: _FakeTracker | None = None):
        self.sent: list[bytes] = []
        self.exit_tracker = tracker or _FakeTracker()
        self._master_fd = 1

    def send(self, data: bytes) -> int:
        self.sent.append(data)
        return len(data)


def test_input_router_routes_semicolon_question_to_ai_handler(capsys):
    pty_manager = _FakePTYManager()
    ai_handler = Mock()
    router = InputRouter(pty_manager, ai_handler)

    router.handle_input(b";hello\r")

    ai_handler.handle_question.assert_called_once_with("hello")
    ai_handler.handle_error_correction.assert_not_called()
    assert pty_manager.sent == []
    assert ";hello" in capsys.readouterr().out


def test_input_router_routes_bare_semicolon_to_error_correction(capsys):
    pty_manager = _FakePTYManager()
    ai_handler = Mock()
    router = InputRouter(pty_manager, ai_handler)

    router.handle_input(b";\r")

    ai_handler.handle_error_correction.assert_called_once_with()
    ai_handler.handle_question.assert_not_called()
    assert pty_manager.sent == []
    assert ";" in capsys.readouterr().out


def test_input_router_marks_normal_command_as_waiting_for_result():
    tracker = _FakeTracker()
    pty_manager = _FakePTYManager(tracker=tracker)
    ai_handler = Mock()
    output_processor = Mock()
    router = InputRouter(pty_manager, ai_handler, output_processor=output_processor)

    router.handle_input(b"ls\r")

    assert pty_manager.sent == [b"l", b"s", b"\r"]
    tracker.set_last_command.assert_called_once_with("ls")
    output_processor.set_waiting_for_result.assert_called_once_with(True, "ls")
    output_processor.set_filter_exit_echo.assert_not_called()


def test_input_router_marks_exit_command_for_echo_filtering():
    tracker = _FakeTracker()
    pty_manager = _FakePTYManager(tracker=tracker)
    ai_handler = Mock()
    output_processor = Mock()
    router = InputRouter(pty_manager, ai_handler, output_processor=output_processor)

    router.handle_input(b"exit\r")

    tracker.set_last_command.assert_called_once_with("exit")
    output_processor.set_filter_exit_echo.assert_called_once_with(True)
    output_processor.set_waiting_for_result.assert_not_called()


def test_output_processor_filters_exit_echo():
    processor = OutputProcessor(_FakePTYManager())
    processor.set_filter_exit_echo(True)

    assert processor.process(b"\rexit\r\n") == b""


def test_output_processor_appends_placeholder_after_prompt():
    placeholder_manager = Mock()
    placeholder_manager.show_placeholder.return_value = b"<hint>"
    processor = OutputProcessor(_FakePTYManager(), placeholder_manager=placeholder_manager)

    rendered = processor.process(b"prompt\x1b[0m ")

    assert rendered.endswith(b"<hint>")
    placeholder_manager.show_placeholder.assert_called_once_with()


def test_output_processor_prints_error_hint_when_command_fails(capsys):
    tracker = _FakeTracker(has_exit_code=True, error_info=("bad command", 1))
    processor = OutputProcessor(_FakePTYManager(tracker=tracker))
    processor._waiting_for_result = True

    rendered = processor.process(b"stderr output")

    assert rendered == b"stderr output"
    assert processor._waiting_for_result is False
    tracker.clear_exit_available.assert_called_once_with()
    assert t("shell.error_correction.press_semicolon_hint") in capsys.readouterr().out