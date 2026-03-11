"""Event adapter for LLM events to TUI updates."""

from typing import TYPE_CHECKING, Callable

from aish.llm import LLMEvent, LLMEventType
from aish.tui.types import ContentLineType

if TYPE_CHECKING:
    from aish.tui.app import TUIApp


class EventAdapter:
    """Adapter to convert LLM events to TUI state updates."""

    def __init__(self, tui_app: "TUIApp"):
        """Initialize event adapter.

        Args:
            tui_app: Reference to TUIApp instance
        """
        self.tui_app = tui_app
        self._current_response = ""

    def create_callback(self) -> Callable[[LLMEvent], None]:
        """Create a callback function for LLM events.

        Returns:
            Callback function to be registered with LLMSession
        """

        def callback(event: LLMEvent) -> None:
            self.handle_event(event)

        return callback

    def handle_event(self, event: LLMEvent) -> None:
        """Handle an LLM event and update TUI state.

        Args:
            event: LLM event to process
        """
        event_type = event.event_type

        if event_type == LLMEventType.OP_START:
            # Operation started - show processing state
            self.tui_app.set_processing(True)
            self.tui_app.set_mode("AI")
            self._current_response = ""

        elif event_type == LLMEventType.OP_END:
            # Operation ended - clear processing state
            self.tui_app.set_processing(False)
            self.tui_app.set_mode("PTY")
            # Flush any remaining response content
            if self._current_response.strip():
                self.tui_app.add_content(
                    self._current_response,
                    ContentLineType.AI_RESPONSE,
                )
            self._current_response = ""

        elif event_type == LLMEventType.GENERATION_START:
            # Generation started
            pass

        elif event_type == LLMEventType.GENERATION_END:
            # Generation ended
            pass

        elif event_type == LLMEventType.CONTENT_DELTA:
            # Streaming content - append to current response
            if event.content:
                self._current_response += event.content
                # Update TUI content in real-time
                self.tui_app.add_content(event.content, ContentLineType.AI_RESPONSE)

        elif event_type == LLMEventType.REASONING_START:
            # Reasoning started
            self.tui_app.add_notification("AI is thinking...", "info")

        elif event_type == LLMEventType.REASONING_DELTA:
            # Reasoning content
            pass

        elif event_type == LLMEventType.REASONING_END:
            # Reasoning ended
            pass

        elif event_type == LLMEventType.TOOL_EXECUTION_START:
            # Tool execution started
            tool_name = event.tool_name or "unknown"
            self.tui_app.add_notification(f"Executing tool: {tool_name}", "info")

        elif event_type == LLMEventType.TOOL_EXECUTION_END:
            # Tool execution ended
            pass

        elif event_type == LLMEventType.TOOL_CONFIRMATION_REQUIRED:
            # Tool needs confirmation
            tool_name = event.tool_name or "unknown"
            self.tui_app.add_notification(
                f"Tool confirmation required: {tool_name}",
                "warning",
            )

        elif event_type == LLMEventType.CANCELLED:
            # Operation cancelled
            self.tui_app.set_processing(False)
            self.tui_app.set_mode("PTY")
            self.tui_app.add_notification("Operation cancelled", "warning")

    def reset(self) -> None:
        """Reset adapter state."""
        self._current_response = ""
