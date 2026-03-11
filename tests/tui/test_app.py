"""Tests for TUIApp main class."""


from aish.tui.app import TUIApp
from aish.tui.types import (
    ContentLine,
    ContentLineType,
    Notification,
    PTYMode,
    StatusInfo,
    TUIEvent,
    TUIState,
)


class TestTUIApp:
    """Test cases for TUIApp."""

    def test_init(self, tui_config, mock_shell):
        """Test TUIApp initialization."""
        app = TUIApp(tui_config, mock_shell)

        assert app.config == tui_config
        assert app.shell == mock_shell
        assert app.tui_settings == tui_config.tui
        assert app._is_running is False
        assert app._pty_mode == PTYMode.CAPTURE

    def test_emit_event(self, tui_app):
        """Test event emission."""
        tui_app.emit_event(TUIEvent.REDRAW)

        # Check event was queued
        event_type, data = tui_app._event_queue.get_nowait()
        assert event_type == TUIEvent.REDRAW
        assert data is None

    def test_update_status(self, tui_app):
        """Test status update."""
        tui_app.update_status(model="new-model", mode="AI", is_processing=True)

        assert tui_app.state.status.model == "new-model"
        assert tui_app.state.status.mode == "AI"
        assert tui_app.state.status.is_processing is True

    def test_add_notification(self, tui_app):
        """Test notification adding."""
        tui_app.add_notification("Test message", level="warning", timeout=10.0)

        assert len(tui_app.state.notifications) == 1
        notif = tui_app.state.notifications[0]
        assert notif.message == "Test message"
        assert notif.level == "warning"
        assert notif.timeout == 10.0

    def test_add_notification_max_limit(self, tui_app):
        """Test notification max limit."""
        tui_app.state.max_notifications = 3

        # Add more notifications than the limit
        for i in range(5):
            tui_app.add_notification(f"Message {i}")

        # Should only have the last 3
        assert len(tui_app.state.notifications) == 3
        assert tui_app.state.notifications[0].message == "Message 2"
        assert tui_app.state.notifications[-1].message == "Message 4"

    def test_add_content(self, tui_app):
        """Test content adding."""
        tui_app.add_content("Line 1\nLine 2", ContentLineType.OUTPUT)

        assert len(tui_app.state.content_lines) == 2
        assert tui_app.state.content_lines[0].text == "Line 1"
        assert tui_app.state.content_lines[1].text == "Line 2"

    def test_add_content_max_lines(self, tui_app):
        """Test content max lines limit."""
        tui_app.state.max_content_lines = 5

        # Add more lines than the limit
        for i in range(10):
            tui_app.add_content(f"Line {i}")

        # Should only have the last 5
        assert len(tui_app.state.content_lines) == 5
        assert tui_app.state.content_lines[0].text == "Line 5"
        assert tui_app.state.content_lines[-1].text == "Line 9"

    def test_set_processing(self, tui_app):
        """Test processing state setting."""
        tui_app.set_processing(True)
        assert tui_app.state.status.is_processing is True

        tui_app.set_processing(False)
        assert tui_app.state.status.is_processing is False

    def test_set_mode(self, tui_app):
        """Test mode setting."""
        tui_app.set_mode("AI")
        assert tui_app.state.status.mode == "AI"

        tui_app.set_mode("PTY")
        assert tui_app.state.status.mode == "PTY"

    def test_set_cwd(self, tui_app):
        """Test CWD setting."""
        tui_app.set_cwd("/home/user/project")
        assert tui_app.state.status.cwd == "/home/user/project"

    def test_pty_mode(self, tui_app):
        """Test PTY mode switching."""
        assert tui_app.pty_mode == PTYMode.CAPTURE

        tui_app.set_pty_mode(PTYMode.PASSTHROUGH)
        assert tui_app.pty_mode == PTYMode.PASSTHROUGH

    def test_append_pty_output_capture(self, tui_app):
        """Test PTY output capture in capture mode."""
        tui_app.set_pty_mode(PTYMode.CAPTURE)
        tui_app.append_pty_output("Test output")

        assert len(tui_app.state.content_lines) == 1
        assert tui_app.state.content_lines[0].text == "Test output"

    def test_stop(self, tui_app):
        """Test stop method."""
        tui_app._is_running = True
        tui_app.stop()

        assert tui_app._is_running is False


class TestTUIState:
    """Test cases for TUIState."""

    def test_init(self):
        """Test TUIState initialization."""
        state = TUIState()

        assert state.status.model == ""
        assert state.status.mode == "PTY"
        assert len(state.notifications) == 0
        assert len(state.content_lines) == 0
        assert state.is_running is True

    def test_add_content_line(self):
        """Test adding content lines."""
        state = TUIState(max_content_lines=5)

        for i in range(10):
            line = ContentLine(text=f"Line {i}")
            state.add_content_line(line)

        assert len(state.content_lines) == 5
        assert state.content_lines[0].text == "Line 5"

    def test_add_notification(self):
        """Test adding notifications."""
        state = TUIState(max_notifications=3)

        for i in range(5):
            notif = Notification(message=f"Message {i}")
            state.add_notification(notif)

        assert len(state.notifications) == 3
        assert state.notifications[0].message == "Message 2"


class TestStatusInfo:
    """Test cases for StatusInfo."""

    def test_init(self):
        """Test StatusInfo initialization."""
        status = StatusInfo()

        assert status.model == ""
        assert status.mode == "PTY"
        assert status.time == ""
        assert status.is_processing is False
        assert status.cwd == ""

    def test_custom_values(self):
        """Test StatusInfo with custom values."""
        status = StatusInfo(
            model="gpt-4",
            mode="AI",
            time="10:30:00",
            is_processing=True,
            cwd="/home/user",
        )

        assert status.model == "gpt-4"
        assert status.mode == "AI"
        assert status.time == "10:30:00"
        assert status.is_processing is True
        assert status.cwd == "/home/user"


class TestNotification:
    """Test cases for Notification."""

    def test_init(self):
        """Test Notification initialization."""
        notif = Notification(message="Test")

        assert notif.message == "Test"
        assert notif.level == "info"
        assert notif.timeout == 5.0
        assert notif.timestamp == 0.0

    def test_custom_values(self):
        """Test Notification with custom values."""
        notif = Notification(
            message="Warning!",
            level="warning",
            timeout=10.0,
            timestamp=12345.0,
        )

        assert notif.message == "Warning!"
        assert notif.level == "warning"
        assert notif.timeout == 10.0
        assert notif.timestamp == 12345.0


class TestContentLine:
    """Test cases for ContentLine."""

    def test_init(self):
        """Test ContentLine initialization."""
        line = ContentLine(text="Test line")

        assert line.text == "Test line"
        assert line.line_type == ContentLineType.OUTPUT
        assert line.timestamp == 0.0

    def test_custom_values(self):
        """Test ContentLine with custom values."""
        line = ContentLine(
            text="Error occurred",
            line_type=ContentLineType.ERROR,
            timestamp=12345.0,
        )

        assert line.text == "Error occurred"
        assert line.line_type == ContentLineType.ERROR
        assert line.timestamp == 12345.0
