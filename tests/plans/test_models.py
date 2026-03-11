"""Tests for Plan models."""


from aish.plans.models import Plan, PlanStatus, PlanStep, StepStatus


class TestPlanStep:
    """Test PlanStep model."""

    def test_create_step(self):
        """Test creating a plan step."""
        step = PlanStep(
            number=1,
            title="Install dependencies",
            description="Install required packages",
            commands=["pip install -r requirements.txt"],
            expected_outcome="All packages installed",
            verification="pip list",
        )

        assert step.number == 1
        assert step.title == "Install dependencies"
        assert step.status == StepStatus.PENDING
        assert len(step.commands) == 1

    def test_step_to_dict(self):
        """Test converting step to dictionary."""
        step = PlanStep(
            number=1,
            title="Test step",
            commands=["echo hello"],
        )

        data = step.to_dict()

        assert data["number"] == 1
        assert data["title"] == "Test step"
        assert data["status"] == "pending"
        assert data["commands"] == ["echo hello"]

    def test_step_from_dict(self):
        """Test creating step from dictionary."""
        data = {
            "number": 1,
            "title": "Test step",
            "description": "Test description",
            "commands": ["echo test"],
            "expected_outcome": "test output",
            "verification": "echo verify",
            "status": "pending",
            "started_at": None,
            "completed_at": None,
            "error_message": "",
            "dependencies": [],
        }

        step = PlanStep.from_dict(data)

        assert step.number == 1
        assert step.title == "Test step"
        assert step.description == "Test description"
        assert step.status == StepStatus.PENDING


class TestPlan:
    """Test Plan model."""

    def test_create_plan(self):
        """Test creating a new plan."""
        plan = Plan.create(
            title="Setup Python project",
            description="Initialize a Python project with proper structure",
            context="New project setup",
        )

        assert plan.plan_id
        assert len(plan.plan_id) == 8  # UUID prefix
        assert plan.title == "Setup Python project"
        assert plan.status == PlanStatus.DRAFT
        assert len(plan.steps) == 0

    def test_add_step_to_plan(self):
        """Test adding steps to a plan."""
        plan = Plan.create(
            title="Test plan",
            description="Test description",
        )

        step1 = PlanStep(number=1, title="First step")
        step2 = PlanStep(number=2, title="Second step")

        plan.steps.extend([step1, step2])

        assert len(plan.steps) == 2
        assert plan.steps[0].number == 1
        assert plan.steps[1].number == 2

    def test_get_step(self):
        """Test getting a step by number."""
        plan = Plan.create(title="Test", description="Test")

        step1 = PlanStep(number=1, title="First")
        step2 = PlanStep(number=2, title="Second")
        plan.steps.extend([step1, step2])

        assert plan.get_step(1) is step1
        assert plan.get_step(2) is step2
        assert plan.get_step(3) is None

    def test_get_next_pending_step(self):
        """Test getting the next pending step."""
        plan = Plan.create(title="Test", description="Test")

        step1 = PlanStep(number=1, title="First", status=StepStatus.COMPLETED)
        step2 = PlanStep(number=2, title="Second", status=StepStatus.PENDING)
        step3 = PlanStep(number=3, title="Third", status=StepStatus.PENDING)

        plan.steps.extend([step1, step2, step3])

        next_step = plan.get_next_pending_step()

        assert next_step is step2

    def test_get_progress_summary(self):
        """Test getting progress summary."""
        plan = Plan.create(title="Test", description="Test")

        step1 = PlanStep(number=1, title="First", status=StepStatus.COMPLETED)
        step2 = PlanStep(number=2, title="Second", status=StepStatus.IN_PROGRESS)
        step3 = PlanStep(number=3, title="Third", status=StepStatus.PENDING)
        step4 = PlanStep(number=4, title="Fourth", status=StepStatus.SKIPPED)
        step5 = PlanStep(number=5, title="Fifth", status=StepStatus.FAILED)

        plan.steps.extend([step1, step2, step3, step4, step5])

        summary = plan.get_progress_summary()

        assert summary["total"] == 5
        assert summary["pending"] == 1
        assert summary["in_progress"] == 1
        assert summary["completed"] == 1
        assert summary["skipped"] == 1
        assert summary["failed"] == 1

    def test_to_markdown(self):
        """Test converting plan to markdown."""
        plan = Plan.create(
            title="Test Plan",
            description="Test description",
        )

        step1 = PlanStep(
            number=1,
            title="First step",
            description="Step description",
            commands=["echo hello"],
            status=StepStatus.COMPLETED,
        )

        plan.steps.append(step1)

        md = plan.to_markdown()

        assert "# Test Plan" in md
        assert "Test description" in md
        assert "First step" in md
        assert "echo hello" in md
        # Check for the checkmark icon (not the rich tags, just the plain icon)
        assert "✓ Step 1" in md
        # Check that status shows completed
        assert "`completed`" in md

    def test_plan_to_dict(self):
        """Test converting plan to dictionary."""
        plan = Plan.create(
            title="Test Plan",
            description="Test description",
        )

        step = PlanStep(number=1, title="Step 1")
        plan.steps.append(step)

        data = plan.to_dict()

        assert data["plan_id"] == plan.plan_id
        assert data["title"] == "Test Plan"
        assert data["status"] == "draft"
        assert len(data["steps"]) == 1

    def test_plan_from_dict(self):
        """Test creating plan from dictionary."""
        data = {
            "plan_id": "test123",
            "title": "Test Plan",
            "description": "Test description",
            "status": "draft",
            "steps": [
                {
                    "number": 1,
                    "title": "Step 1",
                    "description": "",
                    "commands": [],
                    "expected_outcome": "",
                    "verification": "",
                    "status": "pending",
                    "started_at": None,
                    "completed_at": None,
                    "error_message": "",
                    "dependencies": [],
                }
            ],
            "context": "",
            "author": "user",
            "created_at": "2024-01-01T00:00:00",
            "updated_at": "2024-01-01T00:00:00",
            "current_step": 1,
            "file_path": "",
        }

        plan = Plan.from_dict(data)

        assert plan.plan_id == "test123"
        assert plan.title == "Test Plan"
        assert plan.status == PlanStatus.DRAFT
        assert len(plan.steps) == 1
