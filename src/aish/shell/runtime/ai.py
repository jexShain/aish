"""AI interaction handling for the PTY shell runtime."""

from __future__ import annotations

import anyio
import asyncio
import os
import re
import select
import signal
import sys
import time
from typing import TYPE_CHECKING, Optional

from ...context_manager import ContextManager
from ...i18n import t
from ...prompts import PromptManager

if TYPE_CHECKING:
    from ...llm import LLMSession
    from ...pty import PTYManager
    from ...skills import SkillManager
    from .app import PTYAIShell
    from ..ui.interaction import PTYUserInteraction


class AIHandler:
    """Handle AI questions and error correction using LLMSession directly."""

    _SKILL_REF_EXTRACT_RE = re.compile(r"@(\w+)")

    def __init__(
        self,
        pty_manager: "PTYManager",
        llm_session: "LLMSession",
        prompt_manager: PromptManager,
        context_manager: ContextManager,
        skill_manager: "SkillManager",
        user_interaction: "PTYUserInteraction",
        original_termios: Optional[list] = None,
    ):
        self.pty_manager = pty_manager
        self.llm_session = llm_session
        self.prompt_manager = prompt_manager
        self.context_manager = context_manager
        self.skill_manager = skill_manager
        self.user_interaction = user_interaction
        self._original_termios = original_termios
        self.shell: Optional["PTYAIShell"] = None

    def _require_shell(self) -> "PTYAIShell":
        if self.shell is None:
            raise RuntimeError("AIHandler is not attached to a shell instance")
        return self.shell

    def _restore_terminal_for_output(self) -> None:
        """Temporarily restore terminal settings for AI output."""
        if self._original_termios:
            try:
                import termios

                termios.tcsetattr(
                    sys.stdin.fileno(), termios.TCSADRAIN, self._original_termios
                )
            except Exception:
                pass
        sys.stdout.flush()

    def _set_raw_mode(self) -> None:
        """Re-enter raw mode after AI output."""
        if self._original_termios:
            try:
                import tty

                tty.setraw(sys.stdin.fileno())
            except Exception:
                pass

    @staticmethod
    def _try_parse_json_output(response: str) -> Optional[dict]:
        """Try to parse response as JSON command."""
        import json

        json_match = re.search(r'```(?:json)?\s*(\{.*?\})\s*```', response, re.DOTALL)
        if json_match:
            try:
                return json.loads(json_match.group(1))
            except json.JSONDecodeError:
                pass

        try:
            return json.loads(response.strip())
        except json.JSONDecodeError:
            return None

    @staticmethod
    def _run_async_in_thread(coro):
        """Run an async coroutine in a separate thread with its own event loop."""
        import asyncio
        from concurrent.futures import ThreadPoolExecutor

        def run_in_thread():
            loop = asyncio.new_event_loop()
            asyncio.set_event_loop(loop)
            try:
                return loop.run_until_complete(coro)
            finally:
                loop.close()

        with ThreadPoolExecutor(max_workers=1) as pool:
            return pool.submit(run_in_thread).result()

    def _extract_skill_refs(self, text: str) -> list[str]:
        """Extract skill references from text."""
        available = {skill.metadata.name for skill in self.skill_manager.list_skills()}
        if not available:
            return []
        refs: list[str] = []
        seen: set[str] = set()
        for match in self._SKILL_REF_EXTRACT_RE.findall(text):
            name = match.lower()
            if name in available and name not in seen:
                refs.append(name)
                seen.add(name)
        return refs

    def _inject_skill_prefix(self, text: str) -> str:
        """Inject skill prefix into text."""
        refs = self._extract_skill_refs(text)
        if not refs:
            return text
        prefix = " ".join([f"use {name} skill to do this." for name in refs])
        return f"{prefix}\n\n{text}"

    def handle_error_correction(self) -> None:
        """Handle error correction."""
        tracker = self.pty_manager.exit_tracker

        # Debug output
        import sys
        sys.stderr.write(f"[DEBUG handle_error_correction] has_error={tracker.has_error}, last_exit_code={tracker.last_exit_code}, last_command={tracker.last_command!r}\n")
        sys.stderr.flush()

        # Check has_error flag which tracks if last command had non-zero exit
        if not tracker.has_error:
            print("\r\033[KNo previous error to fix.")
            self._trigger_prompt_redraw()
            self._set_raw_mode()
            return

        cmd = tracker.last_command
        if not cmd:
            print("\r\033[KNo previous command to fix.")
            self._trigger_prompt_redraw()
            self._set_raw_mode()
            return

        try:
            self._restore_terminal_for_output()

            async def _fix():
                with self.llm_session.cancellation_token.open_cancel_scope():
                    system_message = self.prompt_manager.substitute_template(
                        "cmd_error",
                        user_nickname=os.getenv("USER", "user"),
                        uname_info=getattr(self, "uname_info", ""),
                        os_info=getattr(self, "os_info", ""),
                        basic_env_info=getattr(self, "basic_env_info", ""),
                        output_language=getattr(self, "output_language", "en"),
                    )

                    prompt = f"""<command_result>
Command: {cmd}
Exit code: {tracker.last_exit_code}
</command_result>"""

                    response = await self.llm_session.completion(
                        prompt,
                        system_message=system_message,
                        emit_events=True,
                        stream=True,
                    )
                    return response

            shell = self._require_shell()
            self.llm_session.reset_cancellation_token()
            shell._user_requested_exit = False

            from ...interruption import ShellState

            shell.interruption_manager.set_state(ShellState.AI_THINKING)
            shell.operation_in_progress = True

            try:
                response = self._run_async_in_thread(_fix())
            except (
                anyio.get_cancelled_exc_class(),
                asyncio.CancelledError,
                KeyboardInterrupt,
            ):
                shell.handle_processing_cancelled()
                return
            finally:
                shell.interruption_manager.set_state(ShellState.NORMAL)
                shell.operation_in_progress = False

            executed_cmd = False
            if response:
                corrected_cmd = self._display_ai_response(response)
                if corrected_cmd:
                    executed_cmd = self._ask_execute_command(corrected_cmd)

            if not executed_cmd:
                self._trigger_prompt_redraw()
                self.pty_manager.send(b"\n")
                max_wait = 0.2
                start_wait = time.time()
                while (time.time() - start_wait) < max_wait:
                    ready, _, _ = select.select(
                        [self.pty_manager._master_fd], [], [], 0.05
                    )
                    if ready:
                        try:
                            data = os.read(self.pty_manager._master_fd, 4096)
                            if data:
                                cleaned = self.pty_manager.exit_tracker.parse_and_update(data)
                                cleaned = cleaned.lstrip(b"\r\n")
                                if cleaned:
                                    sys.stdout.buffer.write(cleaned)
                                    sys.stdout.buffer.flush()
                        except OSError:
                            break

            self._set_raw_mode()

        except Exception as error:
            print(f"\r\033[KError: {error}")
            self._set_raw_mode()

    def handle_question(self, question: str) -> None:
        """Handle AI question."""
        try:
            self._restore_terminal_for_output()

            async def _ask():
                with self.llm_session.cancellation_token.open_cancel_scope():
                    system_message = self.prompt_manager.substitute_template(
                        "oracle",
                        user_nickname=os.getenv("USER", "user"),
                        uname_info=getattr(self, "uname_info", ""),
                        os_info=getattr(self, "os_info", ""),
                        basic_env_info=getattr(self, "basic_env_info", ""),
                        output_language=getattr(self, "output_language", "en"),
                    )

                    question_processed = self._inject_skill_prefix(question)

                    response = await self.llm_session.process_input(
                        question_processed,
                        context_manager=self.context_manager,
                        system_message=system_message,
                        stream=True,
                    )
                    return response

            shell = self._require_shell()
            self.llm_session.reset_cancellation_token()
            shell._user_requested_exit = False

            from ...interruption import ShellState

            shell.interruption_manager.set_state(ShellState.AI_THINKING)
            shell.operation_in_progress = True

            try:
                response = self._run_async_in_thread(_ask())
            except (
                anyio.get_cancelled_exc_class(),
                asyncio.CancelledError,
                KeyboardInterrupt,
            ):
                shell.handle_processing_cancelled()
                return
            finally:
                shell.interruption_manager.set_state(ShellState.NORMAL)
                shell.operation_in_progress = False

            if response:
                self._display_ai_response(response)

            self._trigger_prompt_redraw()
            self.pty_manager.send(b"\n")

            max_wait = 0.2
            start_wait = time.time()
            while (time.time() - start_wait) < max_wait:
                ready, _, _ = select.select([self.pty_manager._master_fd], [], [], 0.05)
                if ready:
                    try:
                        data = os.read(self.pty_manager._master_fd, 4096)
                        if data:
                            cleaned = self.pty_manager.exit_tracker.parse_and_update(data)
                            cleaned = cleaned.lstrip(b"\r\n")
                            if cleaned:
                                sys.stdout.buffer.write(cleaned)
                                sys.stdout.buffer.flush()
                    except OSError:
                        break

            self._set_raw_mode()

        except Exception as error:
            print(f"\r\033[KError: {error}")
            self._set_raw_mode()

    def _trigger_prompt_redraw(self) -> None:
        """Trigger bash to redraw its prompt by sending SIGWINCH."""
        if self.pty_manager._child_pid:
            try:
                os.kill(self.pty_manager._child_pid, signal.SIGWINCH)
            except Exception:
                pass

    def _display_ai_response(self, response: str) -> Optional[str]:
        """Display AI response, handling JSON command format."""
        from rich.box import HORIZONTALS
        from rich.console import Console
        from rich.markdown import Markdown
        from rich.panel import Panel

        console = Console()

        json_cmd = self._try_parse_json_output(response)
        if json_cmd:
            if json_cmd.get("type") == "corrected_command":
                command = json_cmd.get("command", "").strip()
                description = json_cmd.get("description", "")
                if not command:
                    print(f"\033[33m⚠ {t('shell.error_correction.no_valid_command')}\033[0m")
                    if description:
                        clean_desc = description.split("Insufficient context")[0].strip()
                        if clean_desc:
                            print(f"   {clean_desc}")
                    print(f"   \033[36m{t('shell.error_correction.retry_hint')}\033[0m")
                    sys.stdout.flush()
                    sys.stderr.flush()
                    console.show_cursor()
                    return None
                print(
                    f"{t('shell.error_correction.corrected_command_title')} \033[1;36m{command}\033[0m"
                )
                if description:
                    print(f"   {description}")
                return command

            console.print(Panel(Markdown(response), border_style="green", box=HORIZONTALS))
            sys.stdout.flush()
            sys.stderr.flush()
            console.show_cursor()
            return None

        console.print(Panel(Markdown(response), border_style="green", box=HORIZONTALS))
        sys.stdout.flush()
        sys.stderr.flush()
        console.show_cursor()
        return None

    def _ask_execute_command(self, command: str) -> bool:
        """Ask user if they want to execute the corrected command."""
        confirmed = self.user_interaction.get_confirmation(
            f"{t('shell.error_correction.confirm_execute_prefix')}\033[1;36m{command}\033[0m{t('shell.error_correction.confirm_execute_suffix')}"
        )
        if confirmed:
            self.pty_manager.exit_tracker.set_last_command(command)
            self.pty_manager.send((command + "\r").encode())
            try:
                ready, _, _ = select.select([self.pty_manager._master_fd], [], [], 0.1)
                if ready:
                    os.read(self.pty_manager._master_fd, 4096)
            except Exception:
                pass
            return True
        return False