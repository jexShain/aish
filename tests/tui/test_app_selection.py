"""Tests for TUIApp selection state management."""

import pytest

from aish.config import ConfigModel
from aish.tui.app import TUIApp
from aish.tui.types import SelectionState


class TestTUIAppSelection:
    """Test TUIApp selection state management."""

    @pytest.fixture
    def tui_app(self):
        """Create a TUIApp for testing."""
        config = ConfigModel(model="test")
        return TUIApp(config)

    def test_initial_selection_state(self, tui_app):
        """Test initial selection state."""
        state = tui_app.get_selection_state()
        assert isinstance(state, SelectionState)
        assert state.is_active is False
        assert state.options == []
        assert state.selected_index == 0

    def test_show_selection(self, tui_app):
        """Test showing selection UI."""
        tui_app.show_selection(
            prompt="Test prompt",
            options=[
                {"value": "1", "label": "Option 1"},
                {"value": "2", "label": "Option 2"},
            ],
            title="Test",
            default="2",
        )

        state = tui_app.get_selection_state()
        assert state.is_active is True
        assert state.prompt == "Test prompt"
        assert len(state.options) == 2
        assert state.selected_index == 1  # "2" is at index 1

    def test_hide_selection(self, tui_app):
        """Test hiding selection UI."""
        tui_app.show_selection(
            prompt="Test",
            options=[{"value": "1", "label": "One"}],
        )
        assert tui_app.get_selection_state().is_active is True

        tui_app.hide_selection()
        assert tui_app.get_selection_state().is_active is False

    def test_move_selection_down(self, tui_app):
        """Test moving selection down."""
        tui_app.show_selection(
            prompt="Test",
            options=[
                {"value": "1", "label": "One"},
                {"value": "2", "label": "Two"},
            ],
        )

        result = tui_app.move_selection(1)
        assert result is True
        assert tui_app.get_selection_state().selected_index == 1

    def test_move_selection_up(self, tui_app):
        """Test moving selection up."""
        tui_app.show_selection(
            prompt="Test",
            options=[
                {"value": "1", "label": "One"},
                {"value": "2", "label": "Two"},
            ],
            default="2",
        )

        result = tui_app.move_selection(-1)
        assert result is True
        assert tui_app.get_selection_state().selected_index == 0

    def test_move_selection_boundary(self, tui_app):
        """Test selection movement at boundaries."""
        tui_app.show_selection(
            prompt="Test",
            options=[
                {"value": "1", "label": "One"},
                {"value": "2", "label": "Two"},
            ],
        )

        # Try to move up from first position
        result = tui_app.move_selection(-10)
        assert result is False  # Should not move
        assert tui_app.get_selection_state().selected_index == 0

        # Move to last position
        tui_app.move_selection(1)

        # Try to move down from last position
        result = tui_app.move_selection(10)
        assert result is False  # Should not move
        assert tui_app.get_selection_state().selected_index == 1

    def test_get_selected_value(self, tui_app):
        """Test getting selected value."""
        tui_app.show_selection(
            prompt="Test",
            options=[
                {"value": "yes", "label": "Yes"},
                {"value": "no", "label": "No"},
            ],
            default="yes",
        )

        assert tui_app.get_selected_value() == "yes"

        tui_app.move_selection(1)
        assert tui_app.get_selected_value() == "no"

    def test_get_selected_value_empty(self, tui_app):
        """Test getting selected value when no selection."""
        assert tui_app.get_selected_value() is None

    def test_get_selection_render(self, tui_app):
        """Test getting selection render."""
        tui_app.show_selection(
            prompt="Select",
            options=[{"value": "1", "label": "One"}],
            title="Test",
        )

        lines = tui_app.get_selection_render()
        assert len(lines) > 0
        assert any("Test" in line for line in lines)
        assert any("One" in line for line in lines)

    def test_show_selection_with_custom_settings(self, tui_app):
        """Test showing selection with custom settings."""
        tui_app.show_selection(
            prompt="Test",
            options=[{"value": "1", "label": "One"}],
            allow_cancel=False,
            allow_custom_input=True,
        )

        state = tui_app.get_selection_state()
        assert state.allow_cancel is False
        assert state.allow_custom_input is True
