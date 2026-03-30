# Shell_pty Ctrl+C 功能移植设计

## 目标

将 shell.py 中完整的 Ctrl+C 中断处理功能移植到 shell_pty.py，使 PTY 模式下的中断行为与旧版一致。

## 当前状态

### shell.py（旧版，功能完整）

- anyio.open_signal_receiver 接收 SIGINT
- _current_op_scope 操作级 CancelScope
- _user_requested_exit 双击退出
- 上下文感知取消消息（AI_THINKING / SANDBOX_EVAL / COMMAND_EXEC）
- handle_processing_cancelled 区分工具拒绝 vs 用户中断
- _safe_cancel_scope 上下文管理器
- 多处 anyio.get_cancelled_exc_class() 精确捕获

### shell_pty.py（新版，功能缺失）

- signal.signal(SIGINT) 同步信号处理
- 无操作级 CancelScope
- 无双击退出
- 固定取消消息 `<操作已取消>`
- handle_processing_cancelled 仅 print + 停止动画

## 设计

### 1. 信号处理：signal.signal → anyio.open_signal_receiver

移除 _sigint_handler 同步处理器，在主事件循环中添加异步信号接收任务：

```python
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
```

保留 _sigwinch_handler 和 _sigterm_handler 不变。

### 2. 操作级 CancelScope

新增 _current_op_scope 属性和 _safe_cancel_scope() 上下文管理器：

```python
self._current_op_scope: anyio.CancelScope | None = None

@contextmanager
def _safe_cancel_scope(self):
    scope = anyio.CancelScope()
    try:
        entered = scope.__enter__()
    except AssertionError:
        yield None
        return
    self._current_op_scope = entered
    try:
        yield entered
    finally:
        self._current_op_scope = None
        scope.__exit__(None, None, None)
```

所有 AI 操作（LLM 调用、命令执行、错误检测）包裹在 _safe_cancel_scope 中。

### 3. 双击 Ctrl+C 退出

```python
self._user_requested_exit: bool = False
```

主循环中 KeyboardInterrupt 处理逻辑：

- 首次 Ctrl+C（无操作进行中）：设置 _user_requested_exit = True，显示提示
- 再次 Ctrl+C（_user_requested_exit 已为 True）：退出 shell
- 开始新操作时：重置 _user_requested_exit = False

### 4. 上下文感知取消消息

利用已有的 InterruptionManager 和 ShellState：

- AI_THINKING: `<Interrupted received.>`
- SANDBOX_EVAL: `<Stopping... finalizing current task.>`
- COMMAND_EXEC: `<Stopping... finishing current task (this may take a moment)>`
- 工具确认被拒: 不显示取消消息

### 5. handle_processing_cancelled 增强

```python
def handle_processing_cancelled(self, event=None):
    self._stop_animation()
    self._reset_reasoning_state()
    self._last_streaming_accumulated = ""
    self._finalize_content_preview()
    # Clear Rich Live display
    # Check tool_denied vs user interrupt
    # Show context-aware message based on last AI state
    self.interruption_manager.clear_last_ai_state()
```

### 6. anyio 取消异常捕获

在所有异步操作中添加 `anyio.get_cancelled_exc_class()` 捕获：
- AI 查询处理
- 错误检测 LLM 调用
- 命令错误处理
- 命令执行

## 不改动

- _sigwinch_handler
- _sigterm_handler
- InputRouter 中 \x03 基本处理
- CancellationToken 机制

## 测试策略

### DejaGnu 测试（优先）

- Ctrl+C 取消 AI 操作后提示符恢复
- 双击 Ctrl+C 退出 shell
- Ctrl+C 取消正在执行的 bash 命令
- Ctrl+C 在空闲时按一次不退出

### pytest 测试（5 秒超时）

- _safe_cancel_scope 创建/取消
- _user_requested_exit 状态转换
- handle_processing_cancelled 不同状态消息
- CancellationToken 与 CancelScope 集成
