"""Tests for PlanAgent integration."""


import pytest

from aish.plans.models import Plan, PlanStatus
from aish.plans.storage import PlanStorage
from aish.tools.plan_tools import FinalizePlanTool


class TestPlanAgentToolHandling:
    """Test PlanAgent tool execution and event handling."""

    def test_finalize_plan_tool_result(self):
        """Test that FinalizePlanTool returns correct ToolResult with plan data."""
        tool = FinalizePlanTool()

        result = tool(
            title="Test Plan",
            description="Test description",
            steps=[
                {
                    "title": "Step 1",
                    "description": "First step",
                    "commands": ["echo hello"],
                }
            ],
        )

        assert result.ok is True
        assert result.data is not None
        assert "plan" in result.data
        assert result.data["plan"]["title"] == "Test Plan"
        assert len(result.data["plan"]["steps"]) == 1

    def test_plan_data_in_result_data(self):
        """Test that ToolResult.data is accessible and contains plan."""
        tool = FinalizePlanTool()

        result = tool(
            title="Test Plan",
            description="Test description",
            steps=[
                {"title": "Step 1"},
                {"title": "Step 2"},
            ],
        )

        # Verify data field contains the plan
        assert hasattr(result, "data")
        assert result.data is not None
        plan_dict = result.data["plan"]
        assert plan_dict["title"] == "Test Plan"
        assert len(plan_dict["steps"]) == 2

    def test_event_with_result_data(self):
        """Test that LLMEvent includes result_data field."""
        tool = FinalizePlanTool()

        result = tool(
            title="Event Test Plan",
            description="Testing event data",
            steps=[{"title": "Test Step"}],
        )

        # Simulate the event structure
        event_data = {
            "result": result.render_for_llm(),
            "result_meta": {
                "ok": result.ok,
                "code": result.code,
                "meta": result.meta,
            },
            "tool_name": "finalize_plan",
            "result_data": getattr(result, "data", None),
        }

        assert event_data["result_data"] is not None
        assert "plan" in event_data["result_data"]
        assert event_data["result_data"]["plan"]["title"] == "Event Test Plan"

    def test_plan_agent_can_extract_plan_from_event(self):
        """Test that PlanAgent can extract plan data from event result_data."""
        tool = FinalizePlanTool()

        result = tool(
            title="Extraction Test Plan",
            description="Testing data extraction",
            steps=[
                {"title": "Step A", "commands": ["cmd1"]},
                {"title": "Step B", "commands": ["cmd2"]},
            ],
        )

        # Simulate what PlanAgent does in event_proxy_callback
        event_data = {
            "result": result.render_for_llm(),
            "result_meta": {
                "ok": result.ok,
                "code": result.code,
                "meta": result.meta,
            },
            "tool_name": "finalize_plan",
            "result_data": getattr(result, "data", None),
        }

        # Extract plan like PlanAgent does
        result_data = event_data.get("result_data")
        assert result_data is not None
        assert isinstance(result_data, dict)

        finalized_plan = result_data.get("plan")
        assert finalized_plan is not None
        assert finalized_plan["title"] == "Extraction Test Plan"
        assert len(finalized_plan["steps"]) == 2

    def test_plan_from_dict_roundtrip(self):
        """Test that plan can survive dict conversion roundtrip."""
        from aish.plans.models import Plan

        original = Plan.create(
            title="Roundtrip Test",
            description="Testing roundtrip conversion",
        )

        from aish.plans.models import PlanStep

        step1 = PlanStep(
            number=1,
            title="Step 1",
            description="First step",
            commands=["echo test"],
        )
        original.steps.append(step1)

        # Convert to dict (what finalize_plan tool does)
        plan_dict = original.to_dict()

        # Convert back from dict (what PlanAgent does)
        restored = Plan.from_dict(plan_dict)

        assert restored.plan_id == original.plan_id
        assert restored.title == original.title
        assert len(restored.steps) == 1
        assert restored.steps[0].title == "Step 1"


class TestPlanAgentIntegration:
    """Integration tests for PlanAgent with storage."""

    @pytest.fixture
    def temp_storage(self):
        """Create temporary storage for testing."""
        import tempfile
        from pathlib import Path

        with tempfile.TemporaryDirectory() as tmpdir:
            yield PlanStorage(data_dir=Path(tmpdir))

    def test_plan_agent_creates_and_saves_plan(self, temp_storage):
        """Test that PlanAgent creates and saves a plan correctly."""
        tool = FinalizePlanTool()

        # Call finalize_plan
        result = tool(
            title="Integration Test Plan",
            description="Plan for integration testing",
            steps=[
                {
                    "title": "Setup environment",
                    "description": "Install dependencies",
                    "commands": ["pip install -r requirements.txt"],
                },
                {
                    "title": "Run tests",
                    "description": "Execute test suite",
                    "commands": ["pytest"],
                },
            ],
        )

        # Extract plan from result
        plan_dict = result.data["plan"]
        plan = Plan.from_dict(plan_dict)

        # Save plan using storage
        temp_storage.save_plan(plan)

        # Load and verify
        loaded_plan = temp_storage.load_plan(plan.plan_id)
        assert loaded_plan is not None
        assert loaded_plan.title == "Integration Test Plan"
        assert len(loaded_plan.steps) == 2
        assert loaded_plan.steps[0].title == "Setup environment"
        assert loaded_plan.steps[1].title == "Run tests"

    def test_complete_workflow_finalize_to_storage(self, temp_storage):
        """Test complete workflow from finalize_plan tool to storage."""
        # This simulates what happens when LLM calls finalize_plan
        tool = FinalizePlanTool()

        result = tool(
            title="Complete Workflow Plan",
            description="Testing complete workflow",
            steps=[
                {"title": "Initialize", "commands": ["git init"]},
                {"title": "Add files", "commands": ["git add ."]},
                {"title": "Commit", "commands": ["git commit -m 'Initial'"]},
            ],
        )

        # Extract plan (simulating PlanAgent event handling)
        plan_dict = result.data["plan"]
        plan = Plan.from_dict(plan_dict)

        # Save plan
        temp_storage.save_plan(plan)

        # Verify plan can be loaded
        loaded = temp_storage.load_plan(plan.plan_id)
        assert loaded is not None
        assert loaded.status == PlanStatus.DRAFT
        assert loaded.current_step == 1
        assert len(loaded.steps) == 3

        # Verify markdown file was created
        md_path = temp_storage.get_markdown_path(plan.plan_id)
        assert md_path.exists()
        md_content = md_path.read_text()
        assert "Complete Workflow Plan" in md_content
        assert "Initialize" in md_content
