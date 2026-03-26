from __future__ import annotations

import sys
from collections.abc import Callable

from .models import InteractionRequest, InteractionResponse, InteractionStatus


class InteractionService:
    def __init__(
        self,
        renderer: Callable[[InteractionRequest], InteractionResponse],
        is_interactive: Callable[[], bool] | None = None,
    ) -> None:
        self._renderer = renderer
        self._is_interactive = is_interactive or self._default_is_interactive

    @staticmethod
    def _default_is_interactive() -> bool:
        try:
            return sys.stdin.isatty() and sys.stdout.isatty()
        except Exception:
            return False

    def request(self, request: InteractionRequest) -> InteractionResponse:
        if not self._is_interactive():
            return InteractionResponse(
                interaction_id=request.id,
                status=InteractionStatus.UNAVAILABLE,
                reason=InteractionStatus.UNAVAILABLE.value,
            )

        try:
            return self._renderer(request)
        except KeyboardInterrupt:
            raise
        except Exception as exc:
            return InteractionResponse(
                interaction_id=request.id,
                status=InteractionStatus.UNAVAILABLE,
                reason="error",
                metadata={"exception_type": type(exc).__name__},
            )