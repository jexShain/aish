from __future__ import annotations

from collections.abc import Callable

from aish.interaction import (
    AskUserRequestBuilder,
    AskUserInteractionAdapter,
    InteractionKind,
    InteractionRequest,
    InteractionResponse,
    InteractionService,
)
from aish.tools.base import ToolBase
from aish.tools.result import ToolResult


class AskUserTool(ToolBase):
    """Ask the user for structured input.

    Cancellation or unavailable interactive UI MUST pause the task and ask the user
    to decide how to proceed (manual selection or continue with default).
    """

    def __init__(
        self,
        request_interaction: Callable[[InteractionRequest], InteractionResponse],
    ) -> None:
        super().__init__(
            name="ask_user",
            description=(
                "\n".join(
                    [
                        "Ask the user for one of three interaction kinds:",
                        "- single_select: choose exactly one predefined option.",
                        "- text_input: enter free-form text only.",
                        "- choice_or_text: choose a predefined option or enter custom text.",
                        "Returns structured output so callers can distinguish selected options from custom text.",
                        "If the UI is unavailable or the user cancels, the task will pause and require user input.",
                    ]
                )
            ),
            parameters={
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "Optional interaction id. If omitted, one is generated.",
                    },
                    "kind": {
                        "type": "string",
                        "enum": ["single_select", "text_input", "choice_or_text"],
                        "description": "Interaction type: single_select, text_input, or choice_or_text.",
                    },
                    "prompt": {
                        "type": "string",
                        "description": "Question/description shown to the user.",
                    },
                    "options": {
                        "type": "array",
                        "description": "Predefined options. Required for single_select and choice_or_text; omit for text_input.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "value": {"type": "string"},
                                "label": {"type": "string"},
                                "description": {"type": "string"},
                            },
                            "required": ["value", "label"],
                        },
                        "minItems": 1,
                    },
                    "default": {
                        "type": "string",
                        "description": "Default value used when present and valid for the interaction kind.",
                    },
                    "title": {
                        "type": "string",
                        "description": "Optional UI title.",
                    },
                    "required": {
                        "type": "boolean",
                        "description": "Whether answering is required.",
                        "default": True,
                    },
                    "allow_cancel": {
                        "type": "boolean",
                        "description": "Whether user can cancel/ESC.",
                        "default": True,
                    },
                    "metadata": {
                        "type": "object",
                        "description": "Optional metadata carried with the interaction request.",
                        "additionalProperties": True,
                    },
                    "placeholder": {
                        "type": "string",
                        "description": "Placeholder text for text_input, or fallback placeholder for choice_or_text custom input.",
                    },
                    "validation": {
                        "type": "object",
                        "description": "Optional validation config for text input interactions.",
                        "properties": {
                            "required": {"type": "boolean"},
                            "min_length": {"type": "integer"},
                        },
                        "additionalProperties": False,
                    },
                    "custom": {
                        "type": "object",
                        "description": "Custom text entry config for choice_or_text interactions. Do not provide this for single_select.",
                        "properties": {
                            "label": {"type": "string"},
                            "placeholder": {"type": "string"},
                            "submit_mode": {"type": "string"},
                        },
                        "additionalProperties": False,
                    },
                },
                "required": ["kind", "prompt"],
            },
        )
        self._interaction_service = InteractionService(
            renderer=request_interaction
        )

    def __call__(
        self,
        kind: str,
        prompt: str,
        options: list[dict] | None = None,
        default: str | None = None,
        title: str | None = None,
        required: bool = True,
        allow_cancel: bool = True,
        metadata: dict | None = None,
        placeholder: str | None = None,
        validation: dict | None = None,
        custom: dict | None = None,
        id: str | None = None,
    ) -> ToolResult:
        request = AskUserRequestBuilder.from_tool_args(
            kind=kind,
            prompt=prompt,
            options=options,
            default=default,
            title=title,
            required=required,
            allow_cancel=allow_cancel,
            metadata=metadata,
            placeholder=placeholder,
            validation=validation,
            custom=custom,
            interaction_id=id,
        )

        if request.kind not in {
            InteractionKind.SINGLE_SELECT,
            InteractionKind.TEXT_INPUT,
            InteractionKind.CHOICE_OR_TEXT,
        }:
            return ToolResult(
                ok=False,
                output=f"Error: unsupported ask_user kind: {request.kind.value}.",
                meta={"kind": "invalid_args"},
            )

        if not request.options and request.kind != InteractionKind.TEXT_INPUT:
            return ToolResult(
                ok=False,
                output="Error: options must be a non-empty list of {value,label} for selection interactions.",
                meta={"kind": "invalid_args"},
            )

        response = self._interaction_service.request(request)
        return AskUserInteractionAdapter.to_tool_result(request, response)
