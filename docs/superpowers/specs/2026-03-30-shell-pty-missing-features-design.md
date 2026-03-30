# shell_pty.py 缺失功能迁移设计

Date: 2026-03-30

## 背景

shell_pty.py 是新版 shell，使用 PTY 直连 bash 替代旧版 subprocess 执行方式。经分析，以下功能缺失需要迁移：

- Session 管理
- 命令结果加入 LLM 上下文
- OP_END 事件处理

注：SecurityManager 不需要迁移，code_exec.py 工具层已有安全检查。

## 1. Session 管理

### 位置
`PTYAIShell.__init__()` 中添加

### 实现
复用 shell.py 的 `_create_new_session_record()` 方法：
- 创建 SessionRecord + SessionStore（SQLite 持久化）
- 失败时 fallback 到 in-memory SessionRecord
- 调用 `set_session_uuid()` 注入日志系统
- session_uuid 可被 LLM 工具（code_exec.py）用于输出 offload

### 代码量
约 40 行（含 import 和 `_create_new_session_record` 方法）

## 2. 命令结果加入 LLM 上下文

### 格式
与 shell.py 的 `add_to_history()` 完全一致：

```
$ command → ✓ (exit 0)
<stdout>
output here
</stdout>
<stderr>
</stderr>
<return_code>
0
</return_code>
<offload>
{"status":"inline","reason":"not_offloaded"}
</offload>
```

### PTY 特殊处理
- stdout/stderr 在 PTY 中混合，全部记为 stdout，stderr 留空
- strip ANSI 转义码（`\x1b\[[0-9;]*[a-zA-Z]`）
- 去掉 bash 回显的命令文本（buffer 第一行）
- 截断逻辑复用 `_truncate_utf8_preview`（默认 1024 字节）

### 实现机制

**OutputProcessor 中添加输出缓冲**：
1. 新增 `_command_output_buffer: bytearray`
2. `set_waiting_for_result(True)` 时清空 buffer，开始累积
3. `process()` 中将 cleaned 数据追加到 buffer
4. 命令完成时（`has_exit_code()` 为 True），提取 buffer 内容
5. 通过回调通知 PTYAIShell 调用 `add_to_history()`

**PTYAIShell 中添加 `add_to_history()`**：
- 复用 shell.py 的格式化逻辑（XML-like entry）
- 调用 `context_manager.add_memory(MemoryType.SHELL, entry)`
- 不需要 offload 功能（PTY 模式输出直接显示），offload 固定为 `{"status":"inline","reason":"not_offloaded"}`

### 代码量
约 80 行（OutputProcessor 缓冲 + PTYAIShell.add_to_history）

## 3. OP_END 事件处理

### 位置
`PTYAIShell` 的事件路由表和新增方法

### 实现
1. 在 `llm_event_router` 的 handlers 中添加 `LLMEventType.OP_END: self.handle_operation_end`
2. 新增 `handle_operation_end(self, event)` 方法：
   - 停止动画 `_stop_animation()`
   - 重置推理状态 `_reset_reasoning_state()`
   - 清空流式累积器 `_last_streaming_accumulated = ""`
   - 最终化内容预览 `_finalize_content_preview()`
   - 清理 Live display（`current_live.stop()`）

### 代码量
约 15 行

## 影响范围

| 文件 | 修改内容 |
|------|----------|
| `src/aish/shell_pty.py` | Session 初始化、add_to_history、OP_END 处理器 |
| 其他文件 | 无修改 |

总计约 135 行新增代码。
