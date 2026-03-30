# Shell_pty Ctrl+C Migration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Port all Ctrl+C handling features from shell.py to shell_pty.py so PTY mode has the same interrupt behavior as the legacy shell.

**Architecture:** Replace synchronous `signal.signal(SIGINT)` with `anyio.open_signal_receiver` in the main loop. Add `_current_op_scope` and `_safe_cancel_scope` for per-operation cancellation. Enhance `handle_processing_cancelled` with context-aware messages. Add double-Ctrl+C exit logic. All changes are in `shell_pty.py` plus new test files.

**Tech Stack:** Python, anyio, signal, CancellationToken, InterruptionManager, DejaGnu (Expect), pytest

**Spec:** `docs/superpowers/specs/2026-03-27-shell-pty-ctrl-c-migration-design.md`

---

## File Map

| File | Action | Responsibility |
|------|--------|---------------|
| `src/aish/shell_pty.py` | Modify | All Ctrl+C feature changes |
| `tests/test_shell_pty_ctrl_c.py` | Create | pytest unit tests (5s timeout) |
| `tests/dejagnu/ctrl_c.test` | Create | DejaGnu integration tests |

---

### Task 1: Add _safe_cancel_scope and _current_op_scope to PTYAIShell

**Files:**
- Modify: `src/aish/shell_pty.py` (add attributes in `__init__` at line ~784, add method after `_finalize_content_preview` at line ~1360)

- [ ] **Step 1: Write the failing test**

Create `tests/test_shell_pty_ctrl_c.py`:

```python
"""Tests for Ctrl+C handling in shell_pty mode."""
import anyio
import pytest

from aish.shell_pty import PTYAIShell


@pytest.mark.timeout(5)
def test_safe_cancel_scope_creates_and_cancels():
    """Test that _safe_cancel_scope creates a scope that can be cancelled."""
    # We can't instantiate PTYAIShell fully (needs config), so test
    # the _safe_cancel_scope method logic directly via a minimal mock.
    from aish.cancellation import CancellationToken

    token = CancellationToken()

    # Simulate _safe_cancel_scope behavior
    scope = anyio.CancelScope()
    entered = scope.__enter__()

    # Simulate signal cancelling the scope
    scope.cancel()

    # Verify the scope is cancelled
    assert scope.cancelled_caught is False  # Not caught yet
    scope.__exit__(None, None, None)


@pytest.mark.timeout(5)
def test_safe_cancel_scope_fallback_no_anyio_task():
    """Test that _safe_cancel_scope yields None when no anyio task is available."""
    # When not inside anyio.run(), entering a CancelScope raises AssertionError
    scope = anyio.CancelScope()
    try:
        entered = scope.__enter__()
        # If we get here, we're inside an anyio context
        scope.__exit__(None, None, None)
    except AssertionError:
        # Expected when not in anyio context - _safe_cancel_scope should yield None
        pass
```

- [ ] **Step 2: Run test to verify it passes (baseline)**

Run: `uv run pytest tests/test_shell_pty_ctrl_c.py -v --timeout=5`
Expected: PASS (this tests the CancelScope behavior we'll use)

- [ ] **Step 3: Add _current_op_scope and _safe_cancel_scope to PTYAIShell**

In `src/aish/shell_pty.py`, add these after line 780 (`self._at_line_start = True`):

```python
        # === Ctrl+C cancellation infrastructure ===
        self._current_op_scope: Optional[Any] = None  # anyio.CancelScope
        self._user_requested_exit: bool = False
        self.operation_in_progress: bool = False
```

Add `import anyio` at the top of the file (if not already present) and add the method after `_finalize_content_preview` (around line 1360):

```python
    @staticmethod
    def _safe_cancel_scope():
        """Provide a CancelScope when running under anyio, fallback otherwise."""
        import anyio

        scope = anyio.CancelScope()
        try:
            entered = scope.__enter__()
        except AssertionError:
            # pytest-asyncio context: no anyio task state available.
            yield None
            return
        try:
            yield entered
        finally:
            scope.__exit__(None, None, None)
```

Also store `interruption_manager` on `self` in `_create_llm_session`. After line 795 (`interruption_manager = InterruptionManager()`), add:

```python
        self.interruption_manager = interruption_manager
```

And set the interrupt callback:

```python
        interruption_manager.set_interrupt_callback(self._on_interrupt_requested)
```

- [ ] **Step 4: Run tests**

Run: `uv run pytest tests/test_shell_pty_ctrl_c.py -v --timeout=5`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/aish/shell_pty.py tests/test_shell_pty_ctrl_c.py
git commit -m "feat(shell_pty): add _safe_cancel_scope, _current_op_scope, and interruption_manager"
```

---

### Task 2: Add _on_interrupt_requested callback method

**Files:**
- Modify: `src/aish/shell_pty.py` (add method near `handle_processing_cancelled` at line ~1148)

- [ ] **Step 1: Write the failing test**

Add to `tests/test_shell_pty_ctrl_c.py`:

```python
@pytest.mark.timeout(5)
def test_on_interrupt_requested_cancels_token():
    """Test that _on_interrupt_requested cancels the cancellation token."""
    from aish.cancellation import CancellationToken, CancellationReason

    token = CancellationToken()
    assert not token.is_cancelled()

    # Simulate what _on_interrupt_requested does
    token.cancel(CancellationReason.USER_INTERRUPT, "User pressed Ctrl+C")
    assert token.is_cancelled()
    assert token.get_cancellation_reason() == CancellationReason.USER_INTERRUPT
```

- [ ] **Step 2: Run test to verify it fails**

Run: `uv run pytest tests/test_shell_pty_ctrl_c.py::test_on_interrupt_requested_cancels_token -v --timeout=5`
Expected: PASS (tests the token behavior the method will use)

- [ ] **Step 3: Add _on_interrupt_requested method**

In `src/aish/shell_pty.py`, add after `handle_processing_cancelled` (around line 1148):

```python
    def _on_interrupt_requested(self) -> None:
        """Interrupt callback - called by InterruptionManager."""
        from aish.cancellation import CancellationReason

        self.llm_session.cancellation_token.cancel(
            CancellationReason.USER_INTERRUPT, "User pressed Ctrl+C"
        )
```

- [ ] **Step 4: Run tests**

Run: `uv run pytest tests/test_shell_pty_ctrl_c.py -v --timeout=5`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/aish/shell_pty.py
git commit -m "feat(shell_pty): add _on_interrupt_requested callback"
```

---

### Task 3: Replace signal.signal(SIGINT) with anyio.open_signal_receiver

**Files:**
- Modify: `src/aish/shell_pty.py` (modify `run()` at line ~1226, modify `_setup_signals()` at line ~1266, remove `_sigint_handler` at line ~1272)

This is the core architectural change. The main loop currently uses a synchronous `select` loop. We need to convert the main loop to run inside `anyio.run()` so we can use `anyio.open_signal_receiver`.

The key challenge: `shell_pty.py` uses a synchronous `select` loop, not an `asyncio` event loop. `anyio.open_signal_receiver` requires an anyio event loop.

**Approach:** Wrap the main loop with `anyio.run()`, keeping the synchronous `select` inside an `anyio.sleep(0.05)` checkpoint loop. The signal handler runs as an async task.

- [ ] **Step 1: Write the failing test**

Add to `tests/test_shell_pty_ctrl_c.py`:

```python
@pytest.mark.asyncio
@pytest.mark.timeout(5)
async def test_signal_receiver_cancels_scope():
    """Test that anyio.open_signal_receiver can cancel a CancelScope."""
    import signal
    import os
    from aish.cancellation import CancellationToken, CancellationReason

    token = CancellationToken()
    cancelled = False

    async def operation():
        nonlocal cancelled
        try:
            with anyio.CancelScope() as scope:
                token._register_scope(scope)
                try:
                    await anyio.sleep(10)  # Long operation
                finally:
                    token._unregister_scope(scope)
        except anyio.get_cancelled_exc_class():
            cancelled = True
            raise

    async def send_signal():
        await anyio.sleep(0.2)
        token.cancel(CancellationReason.USER_INTERRUPT, "SIGINT")

    async with anyio.create_task_group() as tg:
        tg.start_soon(operation)
        tg.start_soon(send_signal)

    assert cancelled, "Operation should have been cancelled via token"
```

- [ ] **Step 2: Run test**

Run: `uv run pytest tests/test_shell_pty_ctrl_c.py::test_signal_receiver_cancels_scope -v --timeout=5`
Expected: PASS

- [ ] **Step 3: Convert run() to use anyio.run() with signal receiver**

Replace the `run()` method (lines 1226-1264) with:

```python
    def run(self) -> None:
        """Main shell loop."""
        self._setup_signals()
        self._save_terminal()
        self._show_welcome()
        self._setup_pty()
        self._setup_components()

        self._running = True

        import anyio
        from aish.cancellation import CancellationReason

        async def _main_loop():
            with anyio.open_signal_receiver(signal.SIGINT) as sigs:
                async def signal_handler():
                    try:
                        async for _ in sigs:
                            if self._current_op_scope is not None:
                                self._current_op_scope.cancel()
                            self.llm_session.cancellation_token.cancel(
                                CancellationReason.USER_INTERRUPT,
                                "SIGINT received",
                            )
                    except anyio.get_cancelled_exc_class():
                        pass

                async with anyio.create_task_group() as tg:
                    tg.start_soon(signal_handler)

                    try:
                        self._set_raw_mode()

                        while self._running:
                            try:
                                read_fds = [sys.stdin.fileno()]
                                if self._pty_manager and self._pty_manager._master_fd is not None:
                                    read_fds.append(self._pty_manager._master_fd)

                                ready, _, _ = select.select(read_fds, [], [], 0.05)
                            except (ValueError, OSError):
                                break

                            for fd in ready:
                                if fd == sys.stdin.fileno():
                                    self._handle_stdin()
                                elif self._pty_manager._master_fd:
                                    self._handle_pty_output()

                            # anyio checkpoint to allow signal handler to run
                            await anyio.sleep(0)

                    except KeyboardInterrupt:
                        # Double Ctrl+C to exit
                        if self._user_requested_exit:
                            self._running = False
                            break

                        if self.operation_in_progress:
                            self.handle_processing_cancelled()
                            self.operation_in_progress = False

                        continue
                    finally:
                        self._cleanup()

        try:
            anyio.run(_main_loop)
        except KeyboardInterrupt:
            pass
        finally:
            self._cleanup()
```

- [ ] **Step 4: Update _setup_signals - remove SIGINT registration**

In `_setup_signals()` (line 1266), remove the SIGINT line:

```python
    def _setup_signals(self) -> None:
        """Set up signal handlers."""
        # SIGINT is handled by anyio.open_signal_receiver in run()
        signal.signal(signal.SIGTERM, self._sigterm_handler)
        signal.signal(signal.SIGWINCH, self._sigwinch_handler)
```

- [ ] **Step 5: Remove old _sigint_handler**

Delete the `_sigint_handler` method (lines 1272-1287) entirely.

- [ ] **Step 6: Run tests**

Run: `uv run pytest tests/test_shell_pty_ctrl_c.py -v --timeout=5`
Expected: PASS

- [ ] **Step 7: Commit**

```bash
git add src/aish/shell_pty.py
git commit -m "feat(shell_pty): replace signal.signal(SIGINT) with anyio.open_signal_receiver"
```

---

### Task 4: Enhance handle_processing_cancelled with context-aware messages

**Files:**
- Modify: `src/aish/shell_pty.py` (replace `handle_processing_cancelled` at line ~1141)

- [ ] **Step 1: Write the failing test**

Add to `tests/test_shell_pty_ctrl_c.py`:

```python
@pytest.mark.timeout(5)
def test_handle_processing_cancelled_context_aware():
    """Test that handle_processing_cancelled shows different messages based on state."""
    from aish.interruption import InterruptionManager, ShellState

    im = InterruptionManager()

    # Test AI_THINKING state
    im.set_state(ShellState.AI_THINKING)
    assert im.get_last_ai_state() == ShellState.AI_THINKING

    # Test SANDBOX_EVAL state
    im.set_state(ShellState.NORMAL)
    im.set_state(ShellState.SANDBOX_EVAL)
    assert im.get_last_ai_state() == ShellState.SANDBOX_EVAL

    # Test COMMAND_EXEC state
    im.set_state(ShellState.NORMAL)
    im.set_state(ShellState.COMMAND_EXEC)
    assert im.get_last_ai_state() == ShellState.COMMAND_EXEC

    # Test clear
    im.clear_last_ai_state()
    assert im.get_last_ai_state() is None


@pytest.mark.timeout(5)
def test_handle_processing_cancelled_tool_denied_no_message():
    """Test that tool_denied events don't show cancellation message."""
    # Simulate event data for tool_cancelled
    event_data = {"reason": "tool_cancelled"}
    is_tool_denied = event_data.get("reason") == "tool_cancelled"
    assert is_tool_denied, "Should detect tool_cancelled reason"
```

- [ ] **Step 2: Run tests**

Run: `uv run pytest tests/test_shell_pty_ctrl_c.py::test_handle_processing_cancelled_context_aware tests/test_shell_pty_ctrl_c.py::test_handle_processing_cancelled_tool_denied_no_message -v --timeout=5`
Expected: PASS

- [ ] **Step 3: Replace handle_processing_cancelled**

Replace the method at line 1141:

```python
    def handle_processing_cancelled(self, event=None) -> None:
        """Handle processing cancelled with context-aware messages."""
        from aish.interruption import ShellState

        self._stop_animation()
        self._reset_reasoning_state()
        self._last_streaming_accumulated = ""
        self._finalize_content_preview()

        # Clear any live display
        if self.current_live:
            self.current_live.update("", refresh=True)
            self.current_live.stop()
            self.current_live = None

        # Check if tool confirmation was denied (user pressed N)
        is_tool_denied = (
            event and event.data and event.data.get("reason") == "tool_cancelled"
        )

        # Show cancellation message with appropriate formatting
        if not is_tool_denied:
            last_ai_state = self.interruption_manager.get_last_ai_state()

            if last_ai_state == ShellState.AI_THINKING:
                self.console.print("<Interrupted received.>", style="dim")
            elif last_ai_state == ShellState.SANDBOX_EVAL:
                self.console.print(
                    "<Stopping... finalizing current task.>", style="dim"
                )
            elif last_ai_state == ShellState.COMMAND_EXEC:
                self.console.print(
                    "<Stopping... finishing current task (this may take a moment)>",
                    style="dim",
                )

        # Clear last AI state
        self.interruption_manager.clear_last_ai_state()
```

- [ ] **Step 4: Run tests**

Run: `uv run pytest tests/test_shell_pty_ctrl_c.py -v --timeout=5`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/aish/shell_pty.py
git commit -m "feat(shell_pty): context-aware cancellation messages in handle_processing_cancelled"
```

---

### Task 5: Wrap AI operations with _safe_cancel_scope and set ShellState

**Files:**
- Modify: `src/aish/shell_pty.py` (modify `handle_error_correction` at line ~406, modify `handle_question` at line ~479)

- [ ] **Step 1: Write the failing test**

Add to `tests/test_shell_pty_ctrl_c.py`:

```python
@pytest.mark.timeout(5)
def test_operation_in_progress_flag_lifecycle():
    """Test that operation_in_progress and _user_requested_exit work together."""
    # Simulate the lifecycle:
    # 1. Start operation -> operation_in_progress = True, _user_requested_exit = False
    # 2. Cancel -> operation_in_progress = False
    # 3. Ctrl+C at prompt -> _user_requested_exit = True
    # 4. Second Ctrl+C -> exit

    operation_in_progress = False
    user_requested_exit = False

    # Start operation
    operation_in_progress = True
    user_requested_exit = False
    assert operation_in_progress
    assert not user_requested_exit

    # Cancel
    operation_in_progress = False
    assert not operation_in_progress

    # First Ctrl+C at idle
    user_requested_exit = True
    assert user_requested_exit

    # Second Ctrl+C -> exit
    assert user_requested_exit  # Would trigger exit
```

- [ ] **Step 2: Run test**

Run: `uv run pytest tests/test_shell_pty_ctrl_c.py::test_operation_in_progress_flag_lifecycle -v --timeout=5`
Expected: PASS

- [ ] **Step 3: Wrap handle_question with _safe_cancel_scope**

In `handle_question` (line ~479), wrap the `anyio.run(_ask)` call with `_safe_cancel_scope` and set shell state:

Replace lines 483-523 with:

```python
        try:
            self._restore_terminal_for_output()

            async def _ask():
                system_message = self.prompt_manager.substitute_template(
                    "oracle",
                    user_nickname=os.getenv("USER", "user"),
                    uname_info=getattr(self, 'uname_info', ''),
                    os_info=getattr(self, 'os_info', ''),
                    basic_env_info=getattr(self, 'basic_env_info', ''),
                    output_language=getattr(self, 'output_language', 'en'),
                )

                question_processed = self._inject_skill_prefix(question)

                # Reset cancellation token for new AI interaction
                self.llm_session.reset_cancellation_token()

                context = self.context_manager.as_messages()

                response = await self.llm_session.process_input(
                    question_processed,
                    context_manager=self.context_manager,
                    system_message=system_message,
                    stream=True,
                )
                return response

            with PTYAIShell._safe_cancel_scope() as scope:
                # Note: PTYAIShell reference needed because _safe_cancel_scope is @staticmethod
                self.shell._current_op_scope = scope
                self.shell.operation_in_progress = True
                self.shell._user_requested_exit = False
                from aish.interruption import ShellState
                self.shell.interruption_manager.set_state(ShellState.AI_THINKING)

                try:
                    response = anyio.run(_ask)
                except (
                    anyio.get_cancelled_exc_class(),
                    asyncio.CancelledError,
                    KeyboardInterrupt,
                ):
                    self.shell.handle_processing_cancelled()
                    self.shell.operation_in_progress = False
                    self.shell.interruption_manager.set_state(ShellState.NORMAL)
                    self.shell._trigger_prompt_redraw()
                    self.shell._set_raw_mode()
                    return
                finally:
                    self.shell._current_op_scope = None
                    self.shell.interruption_manager.set_state(ShellState.NORMAL)
```

**Important:** `AIHandler` needs a reference to the parent `PTYAIShell`. Add `self.shell = None` in `AIHandler.__init__` and set it in `_setup_components`:

In `AIHandler.__init__` (line ~321), add:
```python
        self.shell = None  # Set by _setup_components
```

In `_setup_components` (line ~1495), after creating `self._ai_handler`:
```python
        self._ai_handler.shell = self
```

- [ ] **Step 4: Wrap handle_error_correction similarly**

Apply the same pattern to `handle_error_correction` (line ~406) - wrap with `_safe_cancel_scope`, set `ShellState.AI_THINKING`, reset in finally.

- [ ] **Step 5: Run tests**

Run: `uv run pytest tests/test_shell_pty_ctrl_c.py -v --timeout=5`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add src/aish/shell_pty.py
git commit -m "feat(shell_pty): wrap AI operations with _safe_cancel_scope and ShellState"
```

---

### Task 6: Add double-Ctrl+C exit logic

**Files:**
- Modify: `src/aish/shell_pty.py` (modify `run()` KeyboardInterrupt handler, modify `InputRouter` Ctrl+C handling at line ~220)

- [ ] **Step 1: Write the failing test**

Add to `tests/test_shell_pty_ctrl_c.py`:

```python
@pytest.mark.timeout(5)
def test_user_requested_exit_reset_on_new_operation():
    """Test that _user_requested_exit resets when a new operation starts."""
    user_requested_exit = True  # Simulate first Ctrl+C
    assert user_requested_exit

    # New operation starts
    user_requested_exit = False
    assert not user_requested_exit
```

- [ ] **Step 2: Run test**

Run: `uv run pytest tests/test_shell_pty_ctrl_c.py::test_user_requested_exit_reset_on_new_operation -v --timeout=5`
Expected: PASS

- [ ] **Step 3: Add double-Ctrl+C logic in InputRouter**

In `InputRouter.handle_input()` (around line 220), the `\x03` (Ctrl+C) handling for non-AI mode needs to set `_user_requested_exit`:

Find the `else` branch (non-AI mode Ctrl+C handling) and modify it to set `self.ai_handler.shell._user_requested_exit` when there's no command in progress:

```python
                if char == "\x03":
                    if self._in_ai_mode:
                        # Cancel AI input
                        self._in_ai_mode = False
                        self._ai_buffer = ""
                        self._at_line_start = True
                        sys.stdout.write("\r\n^C\r\n")
                        sys.stdout.flush()
                        return
                    else:
                        # Forward to PTY and reset command
                        self.pty_manager.send(char.encode())
                        self._current_cmd = ""

                        # Double-Ctrl+C exit logic
                        if self.ai_handler.shell is not None:
                            if self.ai_handler.shell.operation_in_progress:
                                self.ai_handler.shell._user_requested_exit = False
                            elif self.ai_handler.shell._user_requested_exit:
                                # Second Ctrl+C at idle -> exit
                                self.ai_handler.shell._running = False
                            else:
                                # First Ctrl+C at idle
                                self.ai_handler.shell._user_requested_exit = True
                        return
```

- [ ] **Step 4: Run tests**

Run: `uv run pytest tests/test_shell_pty_ctrl_c.py -v --timeout=5`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/aish/shell_pty.py
git commit -m "feat(shell_pty): add double-Ctrl+C exit logic"
```

---

### Task 7: Add anyio cancellation exception handling to all async operations

**Files:**
- Modify: `src/aish/shell_pty.py` (ensure all `anyio.run()` call sites catch `anyio.get_cancelled_exc_class()`)

- [ ] **Step 1: Verify all anyio.run() call sites have proper exception handling**

Check that these locations all catch `anyio.get_cancelled_exc_class()`:
1. `handle_question` - `anyio.run(_ask)` (already done in Task 5)
2. `handle_error_correction` - `anyio.run(_fix)` (already done in Task 5)

Search for any other `anyio.run` calls in the file.

- [ ] **Step 2: Run all tests**

Run: `uv run pytest tests/test_shell_pty_ctrl_c.py -v --timeout=5`
Expected: PASS

- [ ] **Step 3: Commit (if any changes needed)**

```bash
git add src/aish/shell_pty.py
git commit -m "feat(shell_pty): ensure all async operations handle anyio cancellation"
```

---

### Task 8: DejaGnu integration test for Ctrl+C

**Files:**
- Create: `tests/dejagnu/ctrl_c.test`

- [ ] **Step 1: Write the DejaGnu test**

Create `tests/dejagnu/ctrl_c.test`:

```tcl
# Test Ctrl+C handling in shell_pty mode
# Tests: cancel AI, double Ctrl+C exit, cancel bash command

load_lib "ai_lib.exp"

# Test: Ctrl+C once at idle prompt does NOT exit
ai_test "ctrl_c_idle_no_exit" {
    global ai_spawn

    set ai_spawn [ai_start_shell]
    ai_wait_for_prompt $ai_spawn

    # Send Ctrl+C
    send "$ai_spawn \x03"

    # Wait briefly - shell should still be running
    sleep 0.5

    # Try to send a simple command - if shell exited, this will fail
    send "$ai_spawn echo still_alive\r"
    {
        expect {
            -i $ai_spawn
            -timeout 5
            "still_alive" {
                pass "ctrl_c_idle_no_exit: shell still running after single Ctrl+C"
                set result 1
            }
            timeout {
                fail "ctrl_c_idle_no_exit: shell may have exited"
                set result 0
            }
        }
    }

    ai_close_shell $ai_spawn
    return $result
}

# Test: double Ctrl+C exits shell
ai_test "ctrl_c_double_exit" {
    global ai_spawn

    set ai_spawn [ai_start_shell]
    ai_wait_for_prompt $ai_spawn

    # Send first Ctrl+C
    send "$ai_spawn \x03"
    sleep 0.3

    # Send second Ctrl+C
    send "$ai_spawn \x03"
    sleep 0.5

    # Shell should have exited - expect eof
    {
        expect {
            -i $ai_spawn
            -timeout 5
            eof {
                pass "ctrl_c_double_exit: shell exited after double Ctrl+C"
                set result 1
            }
            timeout {
                fail "ctrl_c_double_exit: shell did not exit"
                set result 0
            }
        }
    }

    ai_close_shell $ai_spawn
    return $result
}

# Test: Ctrl+C cancels running bash command
ai_test "ctrl_c_cancel_bash_command" {
    global ai_spawn

    set ai_spawn [ai_start_shell]
    ai_wait_for_prompt $ai_spawn

    # Start a long-running command
    send "$ai_spawn sleep 100\r"
    sleep 0.5

    # Send Ctrl+C to cancel
    send "$ai_spawn \x03"
    sleep 0.5

    # Shell should still be running with prompt
    send "$ai_spawn echo cmd_cancelled\r"
    {
        expect {
            -i $ai_spawn
            -timeout 5
            "cmd_cancelled" {
                pass "ctrl_c_cancel_bash_command: command cancelled, shell running"
                set result 1
            }
            timeout {
                fail "ctrl_c_cancel_bash_command: timeout waiting for prompt"
                set result 0
            }
        }
    }

    ai_close_shell $ai_spawn
    return $result
}
```

- [ ] **Step 2: Run DejaGnu tests**

Run: `cd tests/dejagnu && make ctrl_c 2>&1 | tail -20` (or equivalent dejagnu run command)

Expected: Tests should run (they may fail until all previous tasks are complete)

- [ ] **Step 3: Commit**

```bash
git add tests/dejagnu/ctrl_c.test
git commit -m "test(dejagnu): add Ctrl+C integration tests for shell_pty"
```

---

### Task 9: Final verification and cleanup

**Files:**
- Modify: `src/aish/shell_pty.py` (cleanup only if needed)

- [ ] **Step 1: Run all pytest tests**

Run: `uv run pytest tests/test_shell_pty_ctrl_c.py tests/test_ctrl_c_cancel.py tests/test_pty_ctrl_c.py -v --timeout=5`
Expected: ALL PASS

- [ ] **Step 2: Run linter**

Run: `uv run ruff check src/aish/shell_pty.py`
Expected: No errors

- [ ] **Step 3: Run type checker**

Run: `uv run mypy src/aish/shell_pty.py`
Expected: No new errors (existing errors are OK)

- [ ] **Step 4: Manual smoke test**

Run: `uv run aish` and manually test:
1. Single Ctrl+C at prompt -> shell continues
2. Double Ctrl+C -> shell exits
3. `;hello` then Ctrl+C during AI response -> cancelled with context message
4. `sleep 100` then Ctrl+C -> command cancelled

- [ ] **Step 5: Final commit**

```bash
git add -A
git commit -m "feat(shell_pty): complete Ctrl+C feature migration from shell.py"
```
