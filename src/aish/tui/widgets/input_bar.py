"""Input bar widget for TUI."""

import asyncio
from typing import TYPE_CHECKING, Optional

from prompt_toolkit import PromptSession
from rich.console import RenderableType
from rich.text import Text

if TYPE_CHECKING:
    from aish.config import TUISettings


class InputBar:
    """Input bar widget with history navigation and completion support."""

    def __init__(self, settings: "TUISettings"):
        """Initialize input bar.

        Args:
            settings: TUI settings
        """
        self.settings = settings
        self.theme = settings.theme

        # Input state
        self._current_input = ""
        self._prompt = "$ "

        # Prompt session for interactive input
        self._session: Optional[PromptSession] = None
        self._input_queue: Optional[asyncio.Queue] = None

        # History for up/down navigation
        self._history: list[str] = []
        self._history_index = -1

    def _ensure_queue(self) -> asyncio.Queue:
        """Ensure input queue exists (lazy creation for async context)."""
        if self._input_queue is None:
            self._input_queue = asyncio.Queue()
        return self._input_queue

    def render(self, prompt: str = "$ ", current_input: str = "") -> RenderableType:
        """Render the input bar.

        Args:
            prompt: Input prompt string
            current_input: Current input buffer content

        Returns:
            Renderable content
        """
        text = Text()
        text.append(prompt, style="bold green")
        text.append(current_input)
        text.append("▌", style="blink")  # Cursor

        return text

    async def get_input(self, prompt: str = "$ ") -> str:
        """Get user input asynchronously.

        This method uses prompt_toolkit for rich input features like
        history navigation and auto-completion.

        Args:
            prompt: Input prompt string

        Returns:
            User input string
        """
        try:
            # Create or reuse session
            if self._session is None:
                self._session = PromptSession()

            # Get input using prompt_toolkit
                result = await self._session.prompt_async(prompt)

            # Add to history
            if result.strip():
                self._history.append(result)
                self._history_index = len(self._history)

            return result

        except (KeyboardInterrupt, EOFError):
            raise
        except asyncio.CancelledError:
            raise
        except Exception:
            # Fallback to simple input
            return await self._simple_input(prompt)

    async def _simple_input(self, prompt: str) -> str:
        """Simple input fallback without prompt_toolkit features.

        Args:
            prompt: Input prompt string

        Returns:
            User input string
        """
        # Use asyncio's run_in_executor to avoid blocking
        loop = asyncio.get_event_loop()
        result = await loop.run_in_executor(
            None,
            lambda: input(prompt),
        )

        # Add to history
        if result.strip():
            self._history.append(result)
            self._history_index = len(self._history)

        return result

    def add_to_history(self, command: str) -> None:
        """Add a command to history.

        Args:
            command: Command to add
        """
        if command.strip():
            self._history.append(command)
            # Limit history size
            max_size = self.settings.max_history_display * 5
            if len(self._history) > max_size:
                self._history = self._history[-max_size:]
            self._history_index = len(self._history)

    def get_previous_history(self) -> Optional[str]:
        """Get previous history item.

        Returns:
            Previous command or None if at beginning
        """
        if not self._history or self._history_index <= 0:
            return None

        self._history_index -= 1
        return self._history[self._history_index]

    def get_next_history(self) -> Optional[str]:
        """Get next history item.

        Returns:
            Next command or None if at end
        """
        if not self._history:
            return None

        if self._history_index >= len(self._history) - 1:
            self._history_index = len(self._history)
            return ""

        self._history_index += 1
        return self._history[self._history_index]

    def set_current_input(self, text: str) -> None:
        """Set current input buffer content.

        Args:
            text: Text to set
        """
        self._current_input = text

    def clear_input(self) -> None:
        """Clear current input buffer."""
        self._current_input = ""
        self._history_index = len(self._history)
