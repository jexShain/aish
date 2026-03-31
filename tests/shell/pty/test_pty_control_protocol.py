from __future__ import annotations

import os
import select
import time

from aish.pty.control_protocol import decode_control_chunk
from aish.pty.manager import PTYManager


def test_decode_control_chunk_handles_partial_ndjson_frames():
    first = b'{"version":1,"type":"session_ready","ts":1}\n{"version":1'
    second = b',"type":"prompt_ready","ts":2}\n'

    events, remainder, errors = decode_control_chunk(b"", first)

    assert [event.type for event in events] == ["session_ready"]
    assert remainder == b'{"version":1'
    assert errors == []

    events, remainder, errors = decode_control_chunk(remainder, second)

    assert [event.type for event in events] == ["prompt_ready"]
    assert remainder == b""
    assert errors == []


def test_decode_control_chunk_reports_invalid_lines_without_dropping_valid_events():
    chunk = b'{"version":1,"type":"session_ready","ts":1}\nnot-json\n'

    events, remainder, errors = decode_control_chunk(b"", chunk)

    assert [event.type for event in events] == ["session_ready"]
    assert remainder == b""
    assert len(errors) == 1


def test_pty_manager_emits_backend_control_events():
    manager = PTYManager(use_output_thread=False)
    manager.start()

    try:
        assert manager.control_fd is not None

        deadline = time.time() + 3.0
        buffer = b""
        event_types: list[str] = []

        while time.time() < deadline and not {"session_ready", "prompt_ready"}.issubset(set(event_types)):
            ready, _, _ = select.select([manager.control_fd], [], [], 0.05)
            if not ready:
                continue

            data = os.read(manager.control_fd, 4096)
            if not data:
                break

            events, buffer, errors = decode_control_chunk(buffer, data)
            assert errors == []
            event_types.extend(event.type for event in events)

        assert "session_ready" in event_types
        assert "prompt_ready" in event_types
    finally:
        manager.stop()


def test_pty_manager_emits_command_started_with_command_seq():
    manager = PTYManager(use_output_thread=False)
    manager.start()

    try:
        assert manager.control_fd is not None

        startup_deadline = time.time() + 2.0
        buffer = b""
        while time.time() < startup_deadline:
            ready, _, _ = select.select([manager.control_fd], [], [], 0.05)
            if not ready:
                continue
            data = os.read(manager.control_fd, 4096)
            if not data:
                break
            events, buffer, errors = decode_control_chunk(buffer, data)
            assert errors == []
            if any(event.type == "prompt_ready" for event in events):
                break

        manager.send_command("printf phase1-control-test", command_seq=7)

        deadline = time.time() + 3.0
        command_started = None
        prompt_ready = None

        while time.time() < deadline and (command_started is None or prompt_ready is None):
            ready, _, _ = select.select([manager.control_fd], [], [], 0.05)
            if not ready:
                continue

            data = os.read(manager.control_fd, 4096)
            if not data:
                break

            events, buffer, errors = decode_control_chunk(buffer, data)
            assert errors == []

            for event in events:
                if event.type == "command_started" and event.payload.get("command_seq") == 7:
                    command_started = event
                if event.type == "prompt_ready" and event.payload.get("command_seq") == 7:
                    prompt_ready = event

        assert command_started is not None
        assert command_started.payload.get("command") == "printf phase1-control-test"
        assert prompt_ready is not None
        assert prompt_ready.payload.get("exit_code") == 0
    finally:
        manager.stop()