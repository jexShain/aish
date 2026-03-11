"""TUI type definitions."""

from dataclasses import dataclass, field
from enum import Enum


class TUIEvent(Enum):
    """TUI event types for inter-component communication."""

    REDRAW = "redraw"
    NOTIFICATION = "notification"
    STATUS_UPDATE = "status_update"
    INPUT_SUBMIT = "input_submit"
    QUIT = "quit"
    PTY_OUTPUT = "pty_output"
    CONTENT_APPEND = "content_append"
    PLAN_QUEUE_UPDATE = "plan_queue_update"  # Update plan queue display
    SELECTION_UPDATE = "selection_update"  # Update inline selection UI


class ContentLineType(Enum):
    """Types of content lines in the content area."""

    INPUT = "input"
    OUTPUT = "output"
    ERROR = "error"
    AI_RESPONSE = "ai_response"
    SYSTEM = "system"
    COMMAND = "command"


@dataclass
class StatusInfo:
    """Status bar information."""

    model: str = ""
    mode: str = "PTY"  # PTY / AI
    time: str = ""
    is_processing: bool = False
    cwd: str = ""  # Current working directory
    hint: str = ""  # Right-side hint (e.g., "Use ';' to ask AI")


@dataclass
class Notification:
    """Notification message."""

    message: str
    level: str = "info"  # info, warning, error
    timeout: float = 5.0  # seconds before auto-dismiss
    timestamp: float = 0.0  # when created (for timeout calculation)


@dataclass
class ContentLine:
    """A single line in the content area."""

    text: str
    line_type: ContentLineType = ContentLineType.OUTPUT
    timestamp: float = 0.0


@dataclass
class TUIState:
    """Overall TUI state container."""

    status: StatusInfo = field(default_factory=StatusInfo)
    notifications: list[Notification] = field(default_factory=list)
    content_lines: list[ContentLine] = field(default_factory=list)
    is_running: bool = True
    is_input_focused: bool = True
    scroll_offset: int = 0
    max_content_lines: int = 1000
    max_notifications: int = 5

    def add_content_line(self, line: ContentLine) -> None:
        """Add a content line, respecting max limit."""
        self.content_lines.append(line)
        # Trim old lines if exceeded
        if len(self.content_lines) > self.max_content_lines:
            excess = len(self.content_lines) - self.max_content_lines
            self.content_lines = self.content_lines[excess:]
            # Adjust scroll offset
            if self.scroll_offset > 0:
                self.scroll_offset = max(0, self.scroll_offset - excess)

    def add_notification(self, notification: Notification) -> None:
        """Add a notification, respecting max limit."""
        self.notifications.append(notification)
        # Remove oldest if exceeded
        if len(self.notifications) > self.max_notifications:
            self.notifications = self.notifications[1:]


class PTYMode(Enum):
    """PTY output handling modes."""

    CAPTURE = "capture"  # Capture output to TUI content area
    PASSTHROUGH = "passthrough"  # Direct terminal passthrough for interactive commands


class StepStatus(Enum):
    """Status of a plan step in the queue."""

    PENDING = "pending"
    IN_PROGRESS = "in_progress"
    COMPLETED = "completed"
    SKIPPED = "skipped"
    FAILED = "failed"


@dataclass
class SelectionState:
    """State for inline selection UI (ask_user, plan queue)."""

    # Selection UI state
    is_active: bool = False  # Whether selection mode is active
    prompt: str = ""  # The question/prompt to display
    options: list[dict] = field(default_factory=list)  # Options with value, label
    selected_index: int = 0  # Currently selected option index
    title: str = ""  # Optional title for the selection
    allow_cancel: bool = True  # Whether Escape cancels
    allow_custom_input: bool = False  # Whether custom input is allowed
    custom_input: str = ""  # Custom input value when allow_custom_input is True

    # Display settings
    max_visible_options: int = 5  # Maximum options to display at once
    show_as_inline: bool = True  # Whether to show inline (above status bar) or as modal

    def get_current_option(self) -> dict | None:
        """Get the currently selected option dict."""
        if 0 <= self.selected_index < len(self.options):
            return self.options[self.selected_index]
        return None

    def get_selected_value(self) -> str | None:
        """Get the value of the currently selected option."""
        option = self.get_current_option()
        if option:
            return option.get("value")
        return None

    def move_selection(self, delta: int) -> bool:
        """Move selection by delta and return True if changed."""
        new_index = self.selected_index + delta
        if 0 <= new_index < len(self.options):
            self.selected_index = new_index
            return True
        return False


@dataclass
class PlanQueueState:
    """Plan queue display state."""

    plan_id: str | None = None
    plan_title: str = ""
    steps: list[dict] = field(default_factory=list)
    current_step: int = 0
    total_steps: int = 0
    is_visible: bool = False

    def add_step(self, number: int, title: str, status: StepStatus) -> None:
        """Add or update a step in the queue."""
        # Check if step already exists
        for step in self.steps:
            if step.get("number") == number:
                step["status"] = status
                return

        # Add new step
        self.steps.append({
            "number": number,
            "title": title,
            "status": status,
        })

    def update_step_status(self, number: int, status: StepStatus) -> None:
        """Update status of a specific step."""
        for step in self.steps:
            if step.get("number") == number:
                step["status"] = status
                return

    def get_progress_summary(self) -> tuple[int, int, int]:
        """Get progress summary (completed, total, percent)."""
        completed = sum(1 for s in self.steps if s.get("status") == StepStatus.COMPLETED)
        total = len(self.steps)
        percent = int(completed / total * 100) if total > 0 else 0
        return completed, total, percent
