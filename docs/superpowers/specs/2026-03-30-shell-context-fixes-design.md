# Shell Context and Error Correction Fixes

## Overview

This design document describes fixes for three issues in the aish shell:
1. Adding command execution status to LLM context
2. Bug: Error correction causes abnormal termination
3. History command selection doesn't trigger error correction properly

## Problem Analysis

### Problem 1: Command Execution Status Not in Context

**Current State:**
- Commands are executed via PTY
- Results (stdout, stderr, returncode) are not added to `context_manager`
- LLM cannot reference previous command results

**Solution:**
- Add `add_shell_history` method to `PTYAIShell` class
- Call it after command execution completes in `_handle_pty_output`
- Format matches reference implementation

### Problem 2: Error Correction Abnormal Termination

**Current State:**
- `handle_error_correction` in `ai.py` shows "thinking" then stops
- Missing proper exception handling and state cleanup

**Solution:**
- Add robust exception handling
- Ensure state cleanup in all cases (success, error, cancel)
- Reference: `~/tmp/src/0330_OK_1/aish/shell.py` lines 1346-1434

### Problem 3: History Command Error Correction State Lost

**Current State:**
- When user selects history command via arrow keys
- `SuggestionEngine` returns the command suffix
- `exit_tracker._last_command` is not updated
- Error correction cannot work for history commands

**Solution:**
- Update `exit_tracker._last_command` when history command is accepted
- Modify `router.py` to call `set_last_command`

## Implementation Plan

### File: `src/aish/shell/runtime/app.py`

1. Add `add_shell_history` method:
```python
def add_shell_history(
    self,
    command: str,
    returncode: int,
    stdout: str,
    stderr: str,
    offload: dict[str, Any] | None = None,
) -> None:
    """Add shell execution context for LLM with output previews and offload hints."""
    # Update last exit code
    self._last_exit_code = returncode

    # Truncate output previews
    preview_bytes = 4096
    stdout_preview = self._truncate_preview(stdout or "", preview_bytes)
    stderr_preview = self._truncate_preview(stderr or "", preview_bytes)

    # Build history entry
    import json
    offload_json = json.dumps(offload or {})
    history_entry = "\n".join([
        f"[Shell] {command}",
        f"<returncode>{returncode}</returncode>",
        f"<stdout>{stdout_preview}</stdout>",
        f"<stderr>{stderr_preview}</stderr>",
        f"<offload>{offload_json}</offload>",
    ])

    self.context_manager.add_memory(MemoryType.SHELL, history_entry)
```

2. Modify `_handle_pty_output` to call `add_shell_history` after command completes

### File: `src/aish/shell/runtime/ai.py`

1. Improve `handle_error_correction`:
- Add try/except with proper cleanup
- Ensure `operation_in_progress` is always reset
- Handle cancellation gracefully

### File: `src/aish/shell/runtime/router.py`

1. Modify arrow key handling:
- When `SuggestionEngine.accept()` returns a suffix
- Call `pty_manager.exit_tracker.set_last_command(updated_cmd)`

## Testing

- Test command execution adds to context
- Test error correction doesn't crash
- Test history command error correction works
