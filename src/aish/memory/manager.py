from __future__ import annotations

import datetime as dt
import re
from pathlib import Path

from aish.memory.config import MemoryConfig
from aish.memory.models import MemoryCategory, MemoryEntry


class MemoryManager:
    """Single-file Markdown memory manager backed by MEMORY.md."""

    _SECTION_HEADERS = {
        MemoryCategory.PREFERENCE: "## Preferences",
        MemoryCategory.ENVIRONMENT: "## Environment",
        MemoryCategory.SOLUTION: "## Solutions",
        MemoryCategory.PATTERN: "## Patterns",
        MemoryCategory.OTHER: "## Other",
    }
    _ENTRY_RE = re.compile(
        r"^- \[#(?P<id>\d+)\] (?P<content>.*?)(?: <!-- (?P<meta>.*?) -->)?$"
    )
    _MANUAL_ENTRY_RE = re.compile(r"^- (?P<content>.+)$")
    _META_RE = re.compile(r"(?P<key>[a-z_]+)=(?P<value>[^ ]+)")
    _WORD_RE = re.compile(r"[a-z0-9_./-]+")

    def __init__(self, config: MemoryConfig):
        self.config = config
        self.memory_dir = Path(config.data_dir).expanduser().resolve()
        self.memory_dir.mkdir(parents=True, exist_ok=True)
        self.memory_md = self.memory_dir / "MEMORY.md"
        self._ensure_memory_md()

    def close(self) -> None:
        """Release resources."""

    def store(
        self,
        content: str,
        category: MemoryCategory,
        source: str = "explicit",
        tags: str = "",
        importance: float = 0.5,
    ) -> int:
        _ = tags
        normalized_content = self._normalize_content(content)
        if not normalized_content:
            raise ValueError("Memory content cannot be empty")

        lines = self._read_lines()
        entries = self._parse_entries(lines)
        existing = self._find_duplicate(entries, category, normalized_content)
        if existing is not None:
            return existing.id

        entry_id = max((entry.id for entry in entries if entry.id > 0), default=0) + 1
        created_at = dt.datetime.utcnow().replace(microsecond=0).isoformat()
        entry = MemoryEntry(
            id=entry_id,
            source=source,
            category=category,
            content=normalized_content,
            importance=importance,
            created_at=created_at,
        )
        entry_line = self._format_entry_line(entry)
        updated_lines = self._insert_entry_line(lines, category, entry_line)
        self._write_lines(updated_lines)
        return entry_id

    def recall(self, query: str, limit: int = 5) -> list[MemoryEntry]:
        normalized_query = query.strip().lower()
        if not normalized_query:
            return []

        query_tokens = set(self._tokenize(normalized_query))
        if not query_tokens and len(normalized_query) < 2:
            return []

        scored: list[tuple[int, float, int, MemoryEntry]] = []
        for entry in self._parse_entries(self._read_lines()):
            score = self._score_entry(entry, normalized_query, query_tokens)
            if score <= 0:
                continue
            scored.append((score, entry.importance, entry.id, entry))

        scored.sort(key=lambda item: (item[0], item[1], item[2]), reverse=True)
        return [item[3] for item in scored[:limit]]

    def delete(self, entry_id: int) -> None:
        lines = self._read_lines()
        updated_lines = [
            line for line in lines if not line.startswith(f"- [#{entry_id}] ")
        ]
        self._write_lines(updated_lines)

    def list_recent(self, limit: int = 10) -> list[MemoryEntry]:
        entries = [
            entry for entry in self._parse_entries(self._read_lines()) if entry.id > 0
        ]
        entries.sort(key=lambda entry: entry.id, reverse=True)
        return entries[:limit]

    def get_session_context(self) -> str:
        text = self.memory_md.read_text().strip()
        return text if self._has_stored_entries(text) else ""

    def get_system_prompt_section(self) -> str:
        return (
            "## Memory System\n"
            "You have persistent long-term memory stored in MEMORY.md.\n"
            "1. Before relying on prior preferences, environment details, or project decisions, use the memory tool with action search.\n"
            "2. When the user shares an important durable fact, use the memory tool with action store.\n"
            "3. Keep stored memories short, factual, and reusable. Avoid saving transient chatter.\n"
            f"4. The memory file lives in {self.memory_md}.\n"
        )

    def _ensure_memory_md(self) -> None:
        if self.memory_md.exists():
            return
        self.memory_md.write_text(
            "# Long-term Memory\n\n"
            "Persistent facts about the user, environment, and project.\n\n"
            "## Preferences\n\n"
            "## Environment\n\n"
            "## Solutions\n\n"
            "## Patterns\n\n"
            "## Other\n"
        )

    def _read_lines(self) -> list[str]:
        return self.memory_md.read_text().splitlines()

    def _write_lines(self, lines: list[str]) -> None:
        self.memory_md.write_text("\n".join(lines).rstrip() + "\n")

    def _parse_entries(self, lines: list[str]) -> list[MemoryEntry]:
        entries: list[MemoryEntry] = []
        current_category: MemoryCategory | None = None
        for line in lines:
            category = self._category_for_header(line.strip())
            if category is not None:
                current_category = category
                continue
            if current_category is None:
                continue

            explicit_match = self._ENTRY_RE.match(line)
            if explicit_match:
                meta = self._parse_meta(explicit_match.group("meta") or "")
                entries.append(
                    MemoryEntry(
                        id=int(explicit_match.group("id")),
                        source=meta.get("source", "manual"),
                        category=current_category,
                        content=explicit_match.group("content").strip(),
                        importance=float(meta.get("importance", "0.5")),
                        created_at=meta.get("created_at"),
                    )
                )
                continue

            manual_match = self._MANUAL_ENTRY_RE.match(line)
            if manual_match:
                entries.append(
                    MemoryEntry(
                        id=0,
                        source="manual",
                        category=current_category,
                        content=manual_match.group("content").strip(),
                        importance=0.5,
                    )
                )
        return entries

    def _parse_meta(self, meta_text: str) -> dict[str, str]:
        return {
            match.group("key"): match.group("value")
            for match in self._META_RE.finditer(meta_text)
        }

    def _find_duplicate(
        self,
        entries: list[MemoryEntry],
        category: MemoryCategory,
        content: str,
    ) -> MemoryEntry | None:
        normalized = content.casefold()
        for entry in entries:
            if entry.category == category and entry.content.casefold() == normalized:
                return entry
        return None

    def _insert_entry_line(
        self, lines: list[str], category: MemoryCategory, entry_line: str
    ) -> list[str]:
        header = self._SECTION_HEADERS[category]
        updated_lines = list(lines)
        header_index = next(
            (index for index, line in enumerate(updated_lines) if line.strip() == header),
            None,
        )
        if header_index is None:
            if updated_lines and updated_lines[-1] != "":
                updated_lines.append("")
            updated_lines.extend([header, "", entry_line])
            return updated_lines

        insert_index = header_index + 1
        while (
            insert_index < len(updated_lines)
            and not updated_lines[insert_index].startswith("## ")
        ):
            insert_index += 1

        while insert_index > header_index + 1 and updated_lines[insert_index - 1] == "":
            insert_index -= 1

        updated_lines.insert(insert_index, entry_line)
        if insert_index + 1 >= len(updated_lines) or updated_lines[insert_index + 1] != "":
            updated_lines.insert(insert_index + 1, "")
        return updated_lines

    def _format_entry_line(self, entry: MemoryEntry) -> str:
        meta = (
            f"source={self._sanitize_meta_value(entry.source)} "
            f"importance={entry.importance:.2f} "
            f"created_at={self._sanitize_meta_value(entry.created_at or '')}"
        )
        return f"- [#{entry.id}] {entry.content} <!-- {meta.strip()} -->"

    def _score_entry(
        self,
        entry: MemoryEntry,
        normalized_query: str,
        query_tokens: set[str],
    ) -> int:
        content = entry.content.casefold()
        content_tokens = set(self._tokenize(content))
        overlap = len(query_tokens & content_tokens)
        if normalized_query in content:
            overlap += 5
        if any(token in content for token in query_tokens):
            overlap += 2
        return overlap

    def _tokenize(self, text: str) -> list[str]:
        return self._WORD_RE.findall(text.casefold())

    def _category_for_header(self, line: str) -> MemoryCategory | None:
        for category, header in self._SECTION_HEADERS.items():
            if line == header:
                return category
        return None

    def _has_stored_entries(self, text: str) -> bool:
        return "- [#" in text or bool(re.search(r"^## .*\n\n- ", text, flags=re.MULTILINE))

    def _normalize_content(self, content: str) -> str:
        return " ".join(content.strip().split())

    def _sanitize_meta_value(self, value: str) -> str:
        return value.replace(" ", "_")
