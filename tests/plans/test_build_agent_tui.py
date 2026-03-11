"""Tests for BuildAgent TUI integration."""

from unittest.mock import MagicMock

import pytest

from aish.config import ConfigModel
from aish.plans.build_agent import BuildAgent
from aish.plans.manager import PlanManager
from aish.skills import SkillManager
from aish.tui.app import TUIApp
from aish.tui.types import StepStatus


class TestBuildAgentTUIIntegration:
    """Test BuildAgent integration with TUI."""

    @pytest.fixture
    def build_agent(self):
        """Create a BuildAgent for testing."""
        config = ConfigModel(model="test", api_key="test")
        skill_manager = SkillManager()
        plan_manager = PlanManager()

        # Create mock shell with TUI app
        mock_shell = MagicMock()
        tui_app = TUIApp(config)
        mock_shell._tui_app = tui_app

        agent = BuildAgent(
            config=config,
            model_id="test",
            skill_manager=skill_manager,
            plan_manager=plan_manager,
            shell=mock_shell,
            api_base=None,
            api_key=None,
            parent_event_callback=None,
            cancellation_token=None,
            history_manager=None,
        )

        return agent, tui_app, plan_manager

    def test_plan_queue_visibility(self, build_agent):
        """Test that plan queue can be shown and hidden."""
        agent, tui_app, plan_manager = build_agent

        # Initially hidden
        assert tui_app.get_plan_queue_state().is_visible is False

        # Show plan queue
        steps_data = [
            {"number": 1, "title": "Step 1", "status": "pending"},
            {"number": 2, "title": "Step 2", "status": "pending"},
        ]

        tui_app.show_plan_queue(
            plan_id="test123",
            plan_title="Test Plan",
            steps=steps_data,
            current_step=1,
        )

        state = tui_app.get_plan_queue_state()
        assert state.is_visible is True
        assert state.plan_id == "test123"
        assert state.plan_title == "Test Plan"
        assert len(state.steps) == 2
        assert state.current_step == 1

        # Hide plan queue
        tui_app.hide_plan_queue()
        assert tui_app.get_plan_queue_state().is_visible is False

    def test_update_plan_step(self, build_agent):
        """Test updating plan step status."""
        agent, tui_app, plan_manager = build_agent

        # Show plan queue first
        steps_data = [
            {"number": 1, "title": "Step 1", "status": "pending"},
            {"number": 2, "title": "Step 2", "status": "pending"},
        ]

        tui_app.show_plan_queue(
            plan_id="test123",
            plan_title="Test Plan",
            steps=steps_data,
            current_step=1,
        )

        # Update step 1 to completed
        tui_app.update_plan_step(1, StepStatus.COMPLETED)

        state = tui_app.get_plan_queue_state()
        assert state.steps[0]["status"] == StepStatus.COMPLETED
        assert state.current_step == 2  # Should increment

        # Update step 2 to in_progress
        tui_app.update_plan_step(2, StepStatus.IN_PROGRESS)

        assert state.steps[1]["status"] == StepStatus.IN_PROGRESS

    def test_plan_queue_render(self, build_agent):
        """Test that plan queue can be rendered."""
        agent, tui_app, plan_manager = build_agent

        # Show plan queue with various statuses
        steps_data = [
            {"number": 1, "title": "Completed Step", "status": "completed"},
            {"number": 2, "title": "In Progress Step", "status": "in_progress"},
            {"number": 3, "title": "Pending Step", "status": "pending"},
            {"number": 4, "title": "Failed Step", "status": "failed"},
            {"number": 5, "title": "Skipped Step", "status": "skipped"},
        ]

        tui_app.show_plan_queue(
            plan_id="test456",
            plan_title="Render Test",
            steps=steps_data,
            current_step=2,
        )

        # Get rendered output
        rendered = tui_app.get_plan_queue_render()
        rendered_text = str(rendered)

        assert "Render Test" in rendered_text
        assert "Completed Step" in rendered_text
        assert "✓" in rendered_text  # Completed icon
        assert "◐" in rendered_text  # In progress icon
        assert "○" in rendered_text  # Pending icon

    def test_progress_summary(self, build_agent):
        """Test progress summary calculation."""
        agent, tui_app, plan_manager = build_agent

        steps_data = [
            {"number": 1, "title": "Step 1", "status": "completed"},
            {"number": 2, "title": "Step 2", "status": "completed"},
            {"number": 3, "title": "Step 3", "status": "pending"},
            {"number": 4, "title": "Step 4", "status": "in_progress"},
        ]

        tui_app.show_plan_queue(
            plan_id="test789",
            plan_title="Progress Test",
            steps=steps_data,
            current_step=4,
        )

        completed, total, percent = tui_app.get_plan_queue_state().get_progress_summary()

        assert completed == 2
        assert total == 4
        assert percent == 50  # 2/4 = 50%

    def test_compact_render(self, build_agent):
        """Test compact plan queue render for status bar."""
        agent, tui_app, plan_manager = build_agent

        steps_data = [
            {"number": 1, "title": "Step 1", "status": "completed"},
            {"number": 2, "title": "Step 2", "status": "pending"},
            {"number": 3, "title": "Step 3", "status": "pending"},
        ]

        tui_app.show_plan_queue(
            plan_id="compact123",
            plan_title="Compact Test",
            steps=steps_data,
            current_step=1,
        )

        # Get compact render
        compact = tui_app.get_plan_queue_render_compact()
        compact_text = str(compact)

        assert "Compact Test" in compact_text
        assert "1/3" in compact_text  # Progress
        assert "%" in compact_text  # Percentage
