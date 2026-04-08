from __future__ import annotations

import os
from pathlib import Path
from pydantic import BaseModel, Field


def _default_data_dir() -> str:
    """Resolve default memory directory under the persistent XDG data path."""
    xdg_data_home = os.environ.get("XDG_DATA_HOME")
    if xdg_data_home:
        base_dir = Path(xdg_data_home).expanduser()
    else:
        base_dir = Path.home() / ".local" / "share"
    return str(base_dir / "aish" / "memory")


class MemoryConfig(BaseModel):
    """Configuration for long-term memory system."""

    enabled: bool = Field(default=True, description="Enable long-term memory")
    data_dir: str = Field(
        default_factory=_default_data_dir,
        description="Directory for memory files",
    )
    recall_limit: int = Field(
        default=5, gt=0, description="Max memories returned per recall"
    )
    recall_token_budget: int = Field(
        default=512, gt=0, description="Max tokens injected per recall"
    )
    auto_recall: bool = Field(
        default=True, description="Automatically inject relevant memories before AI turns"
    )
    auto_retain: bool = Field(
        default=True,
        description="Automatically retain explicit durable facts from the user turn",
    )
