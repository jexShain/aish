"""Tools for Plan mode operations."""

from typing import Any

from aish.plans.models import Plan, PlanStep, StepStatus
from aish.tools.base import ToolBase
from aish.tools.result import ToolResult


class FinalizePlanTool(ToolBase):
    """Tool for finalizing a plan during the planning phase."""

    name: str = "finalize_plan"
    description: str = """
    Finalize the planning phase by creating a structured plan with steps.
    Use this tool when you have completed your research and are ready to present the plan.
    """
    parameters: dict = {
        "type": "object",
        "properties": {
            "title": {
                "type": "string",
                "description": "Title of the plan",
            },
            "description": {
                "type": "string",
                "description": "Detailed description of the plan",
            },
            "steps": {
                "type": "array",
                "description": "List of plan steps",
                "items": {
                    "type": "object",
                    "properties": {
                        "title": {"type": "string", "description": "Step title"},
                        "description": {
                            "type": "string",
                            "description": "Detailed description of what this step does",
                        },
                        "commands": {
                            "type": "array",
                            "description": "Commands to execute for this step",
                            "items": {"type": "string"},
                        },
                        "expected_outcome": {
                            "type": "string",
                            "description": "What to expect after this step completes",
                        },
                        "verification": {
                            "type": "string",
                            "description": "How to verify this step completed successfully",
                        },
                        "dependencies": {
                            "type": "array",
                            "description": "List of step numbers this depends on",
                            "items": {"type": "integer"},
                        },
                    },
                    "required": ["title"],
                },
            },
        },
        "required": ["title", "description", "steps"],
    }

    def __call__(
        self,
        title: str,
        description: str,
        steps: list[dict[str, Any]],
    ) -> ToolResult:
        """Finalize the plan.

        Args:
            title: Plan title
            description: Plan description
            steps: List of step definitions

        Returns:
            ToolResult with the finalized plan
        """

        # Create plan object (without saving - that happens in PlanAgent)
        plan = Plan.create(
            title=title,
            description=description,
        )

        # Add steps
        for i, step_data in enumerate(steps, 1):
            step = PlanStep(
                number=i,
                title=step_data.get("title", f"Step {i}"),
                description=step_data.get("description", ""),
                commands=step_data.get("commands", []),
                expected_outcome=step_data.get("expected_outcome", ""),
                verification=step_data.get("verification", ""),
                status=StepStatus.PENDING,
                dependencies=step_data.get("dependencies", []),
            )
            plan.steps.append(step)

        return ToolResult(
            ok=True,
            output=f"Plan finalized with {len(steps)} steps. Please review the plan below.",
            data={
                "plan": plan.to_dict(),
            },
        )


class StepCompleteTool(ToolBase):
    """Tool for marking a step as complete during execution."""

    name: str = "step_complete"
    description: str = """
    Mark the current step as completed and move to the next step.
    Call this when you have successfully completed all commands in the current step.
    """
    parameters: dict = {
        "type": "object",
        "properties": {
            "step_number": {
                "type": "integer",
                "description": "The step number that was completed",
            },
            "notes": {
                "type": "string",
                "description": "Optional notes about the completion",
            },
        },
        "required": ["step_number"],
    }

    def __call__(
        self,
        step_number: int,
        notes: str = "",
    ) -> ToolResult:
        """Mark step as complete.

        Args:
            step_number: The completed step number
            notes: Optional completion notes

        Returns:
            ToolResult indicating success
        """
        return ToolResult(
            ok=True,
            output=f"Step {step_number} marked as complete." + (f" Notes: {notes}" if notes else ""),
            data={
                "step_number": step_number,
                "status": "completed",
                "notes": notes,
            },
        )


class StepSkipTool(ToolBase):
    """Tool for skipping a step during execution."""

    name: str = "step_skip"
    description: str = """
    Skip the current step and move to the next one.
    Use this when a step is not applicable or has been manually completed.
    """
    parameters: dict = {
        "type": "object",
        "properties": {
            "step_number": {
                "type": "integer",
                "description": "The step number to skip",
            },
            "reason": {
                "type": "string",
                "description": "Reason for skipping",
            },
        },
        "required": ["step_number"],
    }

    def __call__(
        self,
        step_number: int,
        reason: str = "",
    ) -> ToolResult:
        """Skip a step.

        Args:
            step_number: The step number to skip
            reason: Reason for skipping

        Returns:
            ToolResult indicating the step was skipped
        """
        return ToolResult(
            ok=True,
            output=f"Step {step_number} skipped." + (f" Reason: {reason}" if reason else ""),
            data={
                "step_number": step_number,
                "status": "skipped",
                "reason": reason,
            },
        )


class StepFailedTool(ToolBase):
    """Tool for marking a step as failed during execution."""

    name: str = "step_failed"
    description: str = """
    Mark the current step as failed due to an error.
    Use this when a step cannot be completed successfully.
    """
    parameters: dict = {
        "type": "object",
        "properties": {
            "step_number": {
                "type": "integer",
                "description": "The step number that failed",
            },
            "error_message": {
                "type": "string",
                "description": "Detailed error message",
            },
            "suggestions": {
                "type": "array",
                "description": "Suggestions for fixing the error",
                "items": {"type": "string"},
            },
        },
        "required": ["step_number", "error_message"],
    }

    def __call__(
        self,
        step_number: int,
        error_message: str,
        suggestions: list[str] | None = None,
    ) -> ToolResult:
        """Mark step as failed.

        Args:
            step_number: The failed step number
            error_message: Error message
            suggestions: Optional suggestions for fixing

        Returns:
            ToolResult indicating the step failed
        """
        return ToolResult(
            ok=False,
            output=f"Step {step_number} failed: {error_message}",
            data={
                "step_number": step_number,
                "status": "failed",
                "error_message": error_message,
                "suggestions": suggestions or [],
            },
        )


class PlanCompleteTool(ToolBase):
    """Tool for marking the entire plan as complete."""

    name: str = "plan_complete"
    description: str = """
    Mark the entire plan as completed successfully.
    Call this when all steps have been completed.
    """
    parameters: dict = {
        "type": "object",
        "properties": {
            "summary": {
                "type": "string",
                "description": "Summary of what was accomplished",
            },
        },
        "required": ["summary"],
    }

    def __call__(self, summary: str) -> ToolResult:
        """Mark plan as complete.

        Args:
            summary: Summary of accomplishments

        Returns:
            ToolResult indicating plan completion
        """
        return ToolResult(
            ok=True,
            output=f"Plan completed successfully. Summary: {summary}",
            data={
                "status": "completed",
                "summary": summary,
            },
        )
