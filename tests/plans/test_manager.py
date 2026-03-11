"""Tests for PlanManager and PlanStorage."""

import tempfile
from pathlib import Path

import pytest

from aish.plans.manager import PlanManager
from aish.plans.models import Plan, PlanStatus, PlanStep, StepStatus


class TestPlanStorage:
    """Test PlanStorage class."""

    @pytest.fixture
    def temp_data_dir(self):
        """Create a temporary directory for test data."""
        with tempfile.TemporaryDirectory() as tmpdir:
            yield Path(tmpdir)

    @pytest.fixture
    def storage(self, temp_data_dir):
        """Create a PlanStorage instance for testing."""
        from aish.plans.storage import PlanStorage

        return PlanStorage(data_dir=temp_data_dir)

    @pytest.fixture
    def sample_plan(self):
        """Create a sample plan for testing."""
        plan = Plan.create(
            title="Test Plan",
            description="Test description",
        )

        step1 = PlanStep(
            number=1,
            title="First step",
            description="First step description",
            commands=["echo hello"],
            expected_outcome="hello printed",
            verification="echo",
        )
        step2 = PlanStep(
            number=2,
            title="Second step",
            description="Second step description",
            commands=["echo world"],
        )

        plan.steps.extend([step1, step2])
        return plan

    def test_save_and_load_plan(self, storage, sample_plan):
        """Test saving and loading a plan."""
        storage.save_plan(sample_plan)

        loaded_plan = storage.load_plan(sample_plan.plan_id)

        assert loaded_plan is not None
        assert loaded_plan.plan_id == sample_plan.plan_id
        assert loaded_plan.title == sample_plan.title
        assert len(loaded_plan.steps) == 2
        assert loaded_plan.steps[0].title == "First step"
        assert loaded_plan.steps[1].title == "Second step"

    def test_save_creates_markdown_file(self, storage, sample_plan, temp_data_dir):
        """Test that saving creates a markdown file."""
        storage.save_plan(sample_plan)

        md_path = temp_data_dir / "plans" / f"{sample_plan.plan_id}.md"
        assert md_path.exists()

        content = md_path.read_text()
        assert sample_plan.title in content
        assert "First step" in content

    def test_list_plans(self, storage, temp_data_dir):
        """Test listing plans."""
        # Create multiple plans
        for i in range(3):
            plan = Plan.create(
                title=f"Plan {i}",
                description=f"Description {i}",
            )
            storage.save_plan(plan)

        plans = storage.list_plans()

        assert len(plans) == 3
        assert plans[0]["title"] == "Plan 2"  # Most recent first
        assert plans[1]["title"] == "Plan 1"
        assert plans[2]["title"] == "Plan 0"

    def test_list_plans_with_status_filter(self, storage):
        """Test listing plans with status filter."""
        # Create plans with different statuses
        plan1 = Plan.create(title="Draft plan", description="Draft")
        plan1.status = PlanStatus.DRAFT
        storage.save_plan(plan1)

        plan2 = Plan.create(title="Approved plan", description="Approved")
        plan2.status = PlanStatus.APPROVED
        storage.save_plan(plan2)

        draft_plans = storage.list_plans(status="draft")
        approved_plans = storage.list_plans(status="approved")

        assert len(draft_plans) == 1
        assert draft_plans[0]["status"] == "draft"
        assert len(approved_plans) == 1
        assert approved_plans[0]["status"] == "approved"

    def test_delete_plan(self, storage, sample_plan, temp_data_dir):
        """Test deleting a plan."""
        storage.save_plan(sample_plan)

        # Verify it exists
        assert storage.load_plan(sample_plan.plan_id) is not None

        # Delete it
        result = storage.delete_plan(sample_plan.plan_id)
        assert result is True

        # Verify it's gone
        assert storage.load_plan(sample_plan.plan_id) is None

        # Verify markdown file is deleted
        md_path = temp_data_dir / "plans" / f"{sample_plan.plan_id}.md"
        assert not md_path.exists()


class TestPlanManager:
    """Test PlanManager class."""

    @pytest.fixture
    def temp_data_dir(self):
        """Create a temporary directory for test data."""
        with tempfile.TemporaryDirectory() as tmpdir:
            yield Path(tmpdir)

    @pytest.fixture
    def manager(self, temp_data_dir):
        """Create a PlanManager instance for testing."""
        return PlanManager(data_dir=temp_data_dir)

    def test_create_plan(self, manager):
        """Test creating a plan through manager."""
        plan = manager.create_plan(
            title="Test Plan",
            description="Test description",
            steps=[
                {
                    "title": "Step 1",
                    "description": "First step",
                    "commands": ["echo hello"],
                },
                {
                    "title": "Step 2",
                    "description": "Second step",
                    "commands": ["echo world"],
                },
            ],
        )

        assert plan.plan_id
        assert plan.title == "Test Plan"
        assert len(plan.steps) == 2
        assert plan.steps[0].title == "Step 1"

        # Verify it was saved
        loaded = manager.load_plan(plan.plan_id)
        assert loaded is not None
        assert loaded.title == "Test Plan"

    def test_update_plan_status(self, manager):
        """Test updating plan status."""
        plan = manager.create_plan(
            title="Test",
            description="Test",
            steps=[{"title": "Step 1"}],
        )

        updated = manager.update_plan_status(plan.plan_id, PlanStatus.APPROVED)

        assert updated is not None
        assert updated.status == PlanStatus.APPROVED

        # Verify it was saved
        loaded = manager.load_plan(plan.plan_id)
        assert loaded.status == PlanStatus.APPROVED

    def test_update_step_status(self, manager):
        """Test updating step status."""
        plan = manager.create_plan(
            title="Test",
            description="Test",
            steps=[
                {"title": "Step 1"},
                {"title": "Step 2"},
            ],
        )

        # Mark first step as completed
        updated = manager.update_step_status(
            plan.plan_id, 1, StepStatus.COMPLETED
        )

        assert updated is not None
        assert updated.steps[0].status == StepStatus.COMPLETED

        # Verify current_step was updated
        assert updated.current_step == 2

    def test_delete_plan(self, manager):
        """Test deleting a plan through manager."""
        plan = manager.create_plan(
            title="Test",
            description="Test",
            steps=[{"title": "Step 1"}],
        )

        result = manager.delete_plan(plan.plan_id)

        assert result is True
        assert manager.load_plan(plan.plan_id) is None

    def test_can_resume_plan(self, manager):
        """Test checking if a plan can be resumed."""
        plan = manager.create_plan(
            title="Test",
            description="Test",
            steps=[{"title": "Step 1"}],
        )

        # Draft plan cannot be resumed
        assert not manager.can_resume_plan(plan.plan_id)

        # Approved plan can be resumed
        manager.update_plan_status(plan.plan_id, PlanStatus.APPROVED)
        assert manager.can_resume_plan(plan.plan_id)

        # In progress plan can be resumed
        manager.update_plan_status(plan.plan_id, PlanStatus.IN_PROGRESS)
        assert manager.can_resume_plan(plan.plan_id)

        # Paused plan can be resumed
        manager.update_plan_status(plan.plan_id, PlanStatus.PAUSED)
        assert manager.can_resume_plan(plan.plan_id)

        # Completed plan cannot be resumed
        manager.update_plan_status(plan.plan_id, PlanStatus.COMPLETED)
        assert not manager.can_resume_plan(plan.plan_id)
