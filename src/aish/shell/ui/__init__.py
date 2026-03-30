"""UI-layer shell components."""

from .interaction import PTYUserInteraction
from .placeholder import PlaceholderManager
from .prompt_io import (
    display_security_panel,
    get_user_confirmation,
    get_user_input,
    handle_interaction_required,
    handle_tool_confirmation_required,
    render_interaction_modal,
)
from .suggestions import SuggestionEngine

__all__ = [
    "PTYUserInteraction",
    "PlaceholderManager",
    "SuggestionEngine",
    "display_security_panel",
    "get_user_confirmation",
    "get_user_input",
    "handle_interaction_required",
    "handle_tool_confirmation_required",
    "render_interaction_modal",
]