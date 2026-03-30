# Ctrl+C Cancellation Fix - Final Summary

## Problem

Ctrl+C 无法中断 AI 响应，出现错误：
```
Error: Not currently running on any asynchronous event loop. Available async backends: asyncio, trio
```

## Root Cause

在 PTY 模式下，AI 操作通过 `anyio.run()` 运行。当添加信号处理时，出现了两个问题：

1. **`anyio.get_cancelled_exc_class()` 在 `anyio.run()` 外部调用**
   - 在 `try/except` 块中使用 `anyio.get_cancelled_exc_class()` 获取异常类
   - 但是在 `anyio.run()` 运行之前就调用它，导致 "Not currently running on any asynchronous event loop" 错误

2. **信号处理器中使用了 `anyio.get_cancelled_exc_class()`**
   - 在信号处理器中直接调用 `anyio.get_cancelled_exc_class()`
   - 这也会导致相同的错误

## Solution

将 `anyio.get_cancelled_exc_class()` 的调用移到 `anyio.run()` 内部的协程中：

### Before (Broken)
```python
async def _fix():
    with anyio.open_signal_receiver(signal.SIGINT) as sigs:
        async def signal_handler():
            async for _ in sigs:
                # ...
                raise anyio.get_cancelled_exc_class()  # ERROR: No event loop

        async with anyio.create_task_group() as tg:
            tg.start_soon(signal_handler)
            # ...

try:
    response = anyio.run(_fix)
except anyio.get_cancelled_exc_class():  # ERROR: No event loop
    # ...
```

### After (Fixed)
```python
async def _fix():
    # Get cancelled exception class INSIDE anyio context
    cancelled_exc = anyio.get_cancelled_exc_class()

    with anyio.open_signal_receiver(signal.SIGINT) as sigs:
        async def signal_handler():
            async for _ in sigs:
                # ...
                raise cancelled_exc()  # OK: Use cached exception class

        async with anyio.create_task_group() as tg:
            tg.start_soon(signal_handler)
            # ...

try:
    # Get cancelled exception class for the outer try/except
    cancelled_exc = anyio.get_cancelled_exc_class()
    response = anyio.run(_fix)
except cancelled_exc:  # OK: Use cached exception class
    # ...
```

## Files Modified

1. **`src/aish/shell_pty.py`**
   - 修改 `handle_error_correction()` 方法中的 `_fix()` 协程
   - 修改 `handle_question()` 方法中的 `_ask()` 协程
   - 将 `anyio.get_cancelled_exc_class()` 的调用移到正确的位置

2. **`src/aish/shell_pty.py` - `_sigint_handler`**
   - 修改 `_sigint_handler` 来取消 AI 操作
   - 在转发 SIGINT 到 PTY 之前先取消 `cancellation_token`

3. **`src/aish/llm.py`** (Previous fix)
   - 在流式响应循环中添加取消检查

4. **`src/aish/tools/code_exec.py`** (Previous fix)
   - 添加 `_CancelEventAdapter` 类
   - 在 `BashTool.__call__` 中添加取消检查

5. **`src/aish/tools/bash_executor.py`** (Previous fix)
   - 将 `subprocess.run()` 替换为 `subprocess.Popen()`
   - 添加轮询循环来检查取消事件

## Testing

所有测试通过：
- `tests/test_anyio_signal.py` - 验证 anyio.run() 中的信号处理
- `tests/test_pty_ctrl_c.py` - 验证 PTY 信号处理器
- `tests/test_ctrl_c_cancel.py` - 基础取消测试
- `tests/verify_ctrl_c_fix.py` - 集成验证测试

## Impact

- Ctrl+C 现在可以正确中断 AI 响应（包括流式响应）
- Ctrl+C 可以中断工具执行（bash 命令）
- PTY 模式下的 AI 操作也可以被 Ctrl+C 中断
- 修复了 "Not currently running on any asynchronous event loop" 错误

## Usage

用户现在可以：
1. 在 AI 响应期间按 Ctrl+C 中断操作
2. 在 bash 命令执行期间按 Ctrl+C 中断命令
3. 在 PTY 模式下使用 Ctrl+C 中断 AI 操作
