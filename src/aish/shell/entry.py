"""Entry points for the shell runtime."""

from __future__ import annotations

from typing import TYPE_CHECKING, Optional

from ..config import Config, ConfigModel
from .runtime.app import PTYAIShell

if TYPE_CHECKING:
    from ..skills import SkillManager


def run_shell(
    config: ConfigModel,
    skill_manager: "SkillManager",
    config_manager: Optional[Config] = None,
) -> None:
    """Run the AI shell (entry point for CLI)."""
    shell = PTYAIShell(config, skill_manager, config_manager)
    try:
        shell.run()
    except KeyboardInterrupt:
        pass
    finally:
        shell._cleanup()