from __future__ import annotations

from aish.llm import LLMCallbackResult, LLMEvent, LLMEventType
from aish.shell.runtime.events import LLMEventRouter


def make_event(event_type: LLMEventType) -> LLMEvent:
    return LLMEvent(event_type=event_type, data={}, timestamp=0.0)


def test_router_returns_continue_when_no_handler():
    router = LLMEventRouter({})
    result = router.handle(make_event(LLMEventType.OP_START))
    assert result == LLMCallbackResult.CONTINUE


def test_router_uses_handler_for_confirmation_event():
    router = LLMEventRouter(
        {
            LLMEventType.TOOL_CONFIRMATION_REQUIRED: lambda _event: LLMCallbackResult.APPROVE,
        }
    )
    result = router.handle(make_event(LLMEventType.TOOL_CONFIRMATION_REQUIRED))
    assert result == LLMCallbackResult.APPROVE


def test_router_uses_handler_for_ask_user_event():
    router = LLMEventRouter(
        {
            LLMEventType.INTERACTION_REQUIRED: lambda _event: LLMCallbackResult.CANCEL,
        }
    )
    result = router.handle(make_event(LLMEventType.INTERACTION_REQUIRED))
    assert result == LLMCallbackResult.CANCEL


def test_router_ignores_non_callback_result_for_ask_user():
    router = LLMEventRouter(
        {
            LLMEventType.INTERACTION_REQUIRED: lambda _event: "ignored",
        }
    )
    result = router.handle(make_event(LLMEventType.INTERACTION_REQUIRED))
    assert result == LLMCallbackResult.CONTINUE
