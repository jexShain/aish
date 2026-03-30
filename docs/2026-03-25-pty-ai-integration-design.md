# PTY Mode AI Integration Design

## Overview

Integrate aish AI features into PTY mode while maintaining perfect bash compatibility.

## User Interaction

### Error Correction Flow
```
:~/path| -> wrongcmd
bash: wrongcmd: 未找到命令
<命令执行失败。输入 ; 后按 Enter 自动分析修复，或直接输入下一条命令。>
:~/path| -> ;     ← User types ; and Enter
[AI analyzes the error and suggests fix...]
```

### AI Chat Flow
```
:~/path| -> ;如何查看大文件
[AI responds...]
```

## Architecture

```
┌─────────────────────────────────────────────┐
│  stdin                                       │
│  ↓                                          │
│  InputRouter                                 │
│  ├─ Line starts with ; → AIHandler          │
│  └─ Otherwise → PTY (bash)                  │
└─────────────────────────────────────────────┘

┌─────────────────────────────────────────────┐
│  PTY Output                                  │
│  ↓                                          │
│  OutputProcessor                             │
│  ├─ Parse [AISH_EXIT:N] marker              │
│  │   └─ N≠0 → Show error hint              │
│  └─ Forward to stdout                       │
└─────────────────────────────────────────────┘

┌─────────────────────────────────────────────┐
│  Screen Layout                               │
│  ┌───────────────────────────────────────┐  │
│  │ Bash output area                      │  │
│  │ :~/path| -> ls                        │  │
│  │ file1 file2                           │  │
│  │ :~/path| -> wrongcmd                  │  │
│  │ bash: wrongcmd: 未找到命令            │  │
│  │ <命令执行失败。输入 ; 纠错...>        │  │ ← Error hint
│  │ :~/path| -> _                         │  │
│  └───────────────────────────────────────┘  │
│  ┌───────────────────────────────────────┐  │
│  │ [Model: ...] [Mode: PTY] [';' AI]     │  │ ← Status bar
│  └───────────────────────────────────────┘  │
└─────────────────────────────────────────────┘
```

## Components

### 1. InputRouter

**Responsibility**: Route user input to PTY or AI handler.

**Logic**:
```python
class InputRouter:
    def __init__(self, pty_manager, ai_handler):
        self.buffer = ""
        self.at_line_start = True

    def handle_input(self, char):
        if self.at_line_start and char == ';':
            # Enter AI mode
            return self._collect_ai_input()
        else:
            # Forward to PTY
            self.at_line_start = (char == '\n' or char == '\r')
            self.pty_manager.send(char)
```

**State tracking**:
- `at_line_start`: True after Enter, False otherwise
- When True and user types `;`, switch to AI input mode

### 2. OutputProcessor

**Responsibility**: Process PTY output, detect errors, show hints.

**Logic**:
```python
class OutputProcessor:
    def process(self, data: bytes) -> bytes:
        # Parse exit code marker
        cleaned, exit_code = self._parse_exit_marker(data)

        # Show error hint if non-zero exit
        if exit_code != 0:
            self._show_error_hint()

        return cleaned

    def _show_error_hint(self):
        # Save cursor, move to new line, print hint, restore cursor
        sys.stdout.write('\n<命令执行失败。输入 ; 后按 Enter 自动分析修复>\n')
```

### 3. StatusBar

**Responsibility**: Display persistent status bar at bottom of screen.

**Content**:
- Model name
- Mode (PTY)
- Current directory
- Quick help (`';' Ask AI`)

**Implementation**:
- Use ANSI escape sequences to position at bottom
- Update on directory change, model change, etc.

```python
class StatusBar:
    def render(self):
        # Save cursor, move to bottom, clear line, render, restore
        lines, cols = get_terminal_size()
        sys.stdout.write(f'\033[{lines};0H')  # Move to bottom
        sys.stdout.write('\033[2K')            # Clear line
        sys.stdout.write(self._format_bar())   # Render content
        sys.stdout.write('\033[u')             # Restore cursor
```

### 4. AIHandler

**Responsibility**: Handle AI questions and error correction.

**Logic**:
```python
class AIHandler:
    async def handle_question(self, question: str):
        # Get AI response using LLMSession
        response = await self.llm_session.completion(question)
        self._display_response(response)

    async def handle_error_correction(self):
        # Get last command and exit code
        cmd = self.exit_tracker.last_command
        code = self.exit_tracker.last_exit_code

        # Build prompt
        prompt = f"Command '{cmd}' failed with exit code {code}. Suggest fix."

        # Get AI response
        response = await self.llm_session.completion(prompt)
        self._display_response(response)
```

### 5. ExitCodeTracker (Existing)

**Already implemented in** `pty/exit_tracker.py`:
- Tracks exit codes via `[AISH_EXIT:N]` marker
- Provides `last_exit_code`, `last_command`, `has_error`

## Implementation Plan

### Phase 1: Input Routing
1. Modify `_forward_stdin()` to buffer input
2. Detect line-start `;` character
3. Route to AI handler or PTY

### Phase 2: Error Detection & Hints
1. Modify `_forward_pty_output()` to detect non-zero exit
2. Display error hint after command failure
3. Track "pending error" state

### Phase 3: Status Bar
1. Implement `StatusBar` class
2. Render on startup
3. Update on events (dir change, etc.)

### Phase 4: AI Integration
1. Implement `AIHandler` class
2. Handle `;question` → AI chat
3. Handle `;` (alone after error) → Error correction

### Phase 5: Ctrl+C Handling
1. Forward SIGINT to PTY (already done)
2. Handle Ctrl+C during AI input (cancel)
3. Double Ctrl+C to exit

## File Structure

```
src/aish/
├── shell_pty.py           # Main shell (modified)
├── pty/
│   ├── manager.py         # PTY manager (existing)
│   ├── exit_tracker.py    # Exit code tracking (existing)
│   └── bash_env.sh        # Bash init script (existing)
└── pty_components/
    ├── __init__.py
    ├── input_router.py    # Input routing
    ├── output_processor.py # Output processing
    ├── status_bar.py      # Status bar
    └── ai_handler.py      # AI handling
```

## Edge Cases

1. **bash's `;` command separator**: Only intercept `;` at line start, not in middle of command
2. **Multi-line input**: Track state across lines
3. **Terminal resize**: Update status bar position
4. **AI response display**: Clear and restore bash prompt area
5. **Ctrl+C during AI**: Cancel AI input, return to bash

## Testing

- Basic bash commands work
- vim/less/top work perfectly
- `;问题` triggers AI chat
- Error after failed command shows hint
- `;` after error triggers correction
- Status bar visible and updates correctly
- Ctrl+C works in all modes
