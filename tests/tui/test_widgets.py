"""Tests for TUI widgets."""

import time

from aish.tui.types import (
    ContentLine,
    ContentLineType,
    Notification,
    StatusInfo,
)
from aish.tui.widgets.status_bar import StatusBar
from aish.tui.widgets.content_area import ContentArea
from aish.tui.widgets.notification_bar import NotificationBar


class TestStatusBar:
    """Test cases for StatusBar widget."""

    def test_init(self, tui_settings):
        """Test StatusBar initialization."""
        bar = StatusBar(tui_settings)
        assert bar.settings == tui_settings
        assert bar.theme == tui_settings.theme

    def test_render_basic(self, tui_settings):
        """Test basic status bar rendering."""
        bar = StatusBar(tui_settings)
        status = StatusInfo(model="test-model", mode="PTY")

        result = bar.render(status)

        # Result should be a Rich Text object
        assert result is not None

    def test_render_with_processing(self, tui_settings):
        """Test status bar rendering with processing indicator."""
        bar = StatusBar(tui_settings)
        status = StatusInfo(model="test-model", mode="AI", is_processing=True)

        result = bar.render(status)
        assert result is not None

    def test_render_with_cwd(self, tui_settings):
        """Test status bar rendering with CWD."""
        bar = StatusBar(tui_settings)
        status = StatusInfo(model="test-model", cwd="/home/user/project")

        result = bar.render(status)
        assert result is not None

    def test_render_long_model_name(self, tui_settings):
        """Test status bar with long model name."""
        bar = StatusBar(tui_settings)
        status = StatusInfo(model="very-long-model-name-that-should-be-truncated")

        result = bar.render(status)
        assert result is not None

    def test_render_with_hint(self, tui_settings):
        """Test status bar rendering with hint."""
        bar = StatusBar(tui_settings)
        status = StatusInfo(model="test-model", mode="PTY")

        result = bar.render(status, hint="Use ';' to ask AI")
        assert result is not None

    def test_render_with_long_hint(self, tui_settings):
        """Test status bar rendering with long hint."""
        bar = StatusBar(tui_settings)
        status = StatusInfo(model="test-model", mode="PTY")

        result = bar.render(status, hint="This is a very long hint message that might need to be displayed")
        assert result is not None


class TestContentArea:
    """Test cases for ContentArea widget."""

    def test_init(self, tui_settings):
        """Test ContentArea initialization."""
        area = ContentArea(tui_settings)
        assert area.settings == tui_settings
        assert area.theme == tui_settings.theme

    def test_render_empty(self, tui_settings):
        """Test content area rendering with no content."""
        area = ContentArea(tui_settings)
        result = area.render([])

        # Should show placeholder
        assert result is not None

    def test_render_with_content(self, tui_settings):
        """Test content area rendering with content."""
        area = ContentArea(tui_settings)
        lines = [
            ContentLine(text="$ ls -la", line_type=ContentLineType.INPUT),
            ContentLine(text="total 64", line_type=ContentLineType.OUTPUT),
            ContentLine(text="drwxr-xr-x 2 user user 4096 Jan 1 10:00 .", line_type=ContentLineType.OUTPUT),
        ]

        result = area.render(lines)
        assert result is not None

    def test_render_with_scroll(self, tui_settings):
        """Test content area rendering with scroll offset."""
        area = ContentArea(tui_settings)
        lines = [ContentLine(text=f"Line {i}") for i in range(100)]

        result = area.render(lines, scroll_offset=50)
        assert result is not None

    def test_render_with_max_height(self, tui_settings):
        """Test content area rendering with max height."""
        area = ContentArea(tui_settings)
        lines = [ContentLine(text=f"Line {i}") for i in range(100)]

        result = area.render(lines, max_height=20)
        assert result is not None

    def test_render_error_line(self, tui_settings):
        """Test content area rendering with error line."""
        area = ContentArea(tui_settings)
        lines = [
            ContentLine(text="$ invalid-command", line_type=ContentLineType.INPUT),
            ContentLine(text="command not found", line_type=ContentLineType.ERROR),
        ]

        result = area.render(lines)
        assert result is not None

    def test_render_ai_response(self, tui_settings):
        """Test content area rendering with AI response."""
        area = ContentArea(tui_settings)
        lines = [
            ContentLine(text="What is Python?", line_type=ContentLineType.INPUT),
            ContentLine(text="Python is a programming language.", line_type=ContentLineType.AI_RESPONSE),
        ]

        result = area.render(lines)
        assert result is not None

    def test_get_line_style(self, tui_settings):
        """Test line style retrieval."""
        area = ContentArea(tui_settings)

        assert area.get_line_style(ContentLineType.INPUT) == "bold cyan"
        assert area.get_line_style(ContentLineType.OUTPUT) == ""
        assert area.get_line_style(ContentLineType.ERROR) == "bold red"
        assert area.get_line_style(ContentLineType.AI_RESPONSE) == "green"

    def test_get_line_prefix(self, tui_settings):
        """Test line prefix retrieval."""
        area = ContentArea(tui_settings)

        assert area.get_line_prefix(ContentLineType.INPUT) == "$ "
        assert area.get_line_prefix(ContentLineType.OUTPUT) == ""
        assert area.get_line_prefix(ContentLineType.ERROR) == "❌ "
        assert area.get_line_prefix(ContentLineType.AI_RESPONSE) == "🤖 "


class TestNotificationBar:
    """Test cases for NotificationBar widget."""

    def test_init(self, tui_settings):
        """Test NotificationBar initialization."""
        bar = NotificationBar(tui_settings)
        assert bar.settings == tui_settings
        assert bar.theme == tui_settings.theme

    def test_render_empty(self, tui_settings):
        """Test notification bar rendering with no notifications."""
        bar = NotificationBar(tui_settings)
        result = bar.render([])

        # Should be empty
        assert result is not None

    def test_render_single_notification(self, tui_settings):
        """Test notification bar rendering with single notification."""
        bar = NotificationBar(tui_settings)
        notifications = [
            Notification(message="Test notification", level="info", timestamp=time.time()),
        ]

        result = bar.render(notifications)
        assert result is not None

    def test_render_warning_notification(self, tui_settings):
        """Test notification bar rendering with warning."""
        bar = NotificationBar(tui_settings)
        notifications = [
            Notification(message="Warning message", level="warning", timestamp=time.time()),
        ]

        result = bar.render(notifications)
        assert result is not None

    def test_render_error_notification(self, tui_settings):
        """Test notification bar rendering with error."""
        bar = NotificationBar(tui_settings)
        notifications = [
            Notification(message="Error occurred", level="error", timestamp=time.time()),
        ]

        result = bar.render(notifications)
        assert result is not None

    def test_render_multiple_notifications(self, tui_settings):
        """Test notification bar rendering with multiple notifications."""
        bar = NotificationBar(tui_settings)
        notifications = [
            Notification(message="First", level="info", timestamp=time.time()),
            Notification(message="Second", level="warning", timestamp=time.time()),
            Notification(message="Third", level="error", timestamp=time.time()),
        ]

        # Should show the latest one
        result = bar.render(notifications)
        assert result is not None

    def test_render_all(self, tui_settings):
        """Test rendering all notifications."""
        bar = NotificationBar(tui_settings)
        notifications = [
            Notification(message="First", level="info", timestamp=time.time()),
            Notification(message="Second", level="warning", timestamp=time.time()),
        ]

        result = bar.render_all(notifications)
        assert result is not None
