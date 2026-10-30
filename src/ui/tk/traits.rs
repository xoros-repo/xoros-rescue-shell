//! The `View` trait and the builder extensions every view gets (`with_name`, sizes).

use std::any::Any;

use super::views::{NamedView, Resized, SizeConstraint};
use super::{Direction, Event, EventResult, Printer, Vec2};

/// Something on the screen: measured, laid out, drawn, and fed key events when focused.
pub trait View: Any {
    fn draw(&self, printer: &Printer);

    /// The size the view wants within `constraint`. May be larger than the constraint; the
    /// parent clips.
    fn required_size(&mut self, constraint: Vec2) -> Vec2;

    /// The final size, before `draw`.
    fn layout(&mut self, _size: Vec2) {}

    fn on_event(&mut self, _event: &Event) -> EventResult {
        EventResult::Ignored
    }

    /// Take the keyboard focus, entering from `dir`; false for views that cannot have it.
    fn take_focus(&mut self, _dir: Direction) -> bool {
        false
    }

    /// Call `f` with the view named `name`, if it is this view or inside it.
    fn call_on_any(&mut self, _name: &str, _f: &mut dyn FnMut(&mut dyn Any)) {}

    /// Row of the focused part (a list's selected item), so a scroll view keeps it visible.
    fn important_row(&self) -> Option<usize> {
        None
    }
}

impl View for Box<dyn View> {
    fn draw(&self, printer: &Printer) {
        (**self).draw(printer)
    }
    fn required_size(&mut self, constraint: Vec2) -> Vec2 {
        (**self).required_size(constraint)
    }
    fn layout(&mut self, size: Vec2) {
        (**self).layout(size)
    }
    fn on_event(&mut self, event: &Event) -> EventResult {
        (**self).on_event(event)
    }
    fn take_focus(&mut self, dir: Direction) -> bool {
        (**self).take_focus(dir)
    }
    fn call_on_any(&mut self, name: &str, f: &mut dyn FnMut(&mut dyn Any)) {
        (**self).call_on_any(name, f)
    }
    fn important_row(&self) -> Option<usize> {
        (**self).important_row()
    }
}

/// Give a view a name for [`super::Ui::call_on_name`].
pub trait Nameable: View + Sized {
    fn with_name(self, name: impl Into<String>) -> NamedView<Self> {
        NamedView::new(name, self)
    }
}

impl<V: View> Nameable for V {}

/// Size limits around a view.
pub trait Resizable: View + Sized {
    fn max_width(self, w: usize) -> Resized<Self> {
        Resized::new(SizeConstraint::AtMost(w), SizeConstraint::Free, self)
    }
    fn max_height(self, h: usize) -> Resized<Self> {
        Resized::new(SizeConstraint::Free, SizeConstraint::AtMost(h), self)
    }
    fn fixed_width(self, w: usize) -> Resized<Self> {
        Resized::new(SizeConstraint::Fixed(w), SizeConstraint::Free, self)
    }
    fn fixed_height(self, h: usize) -> Resized<Self> {
        Resized::new(SizeConstraint::Free, SizeConstraint::Fixed(h), self)
    }
    fn full_width(self) -> Resized<Self> {
        Resized::new(SizeConstraint::Full, SizeConstraint::Free, self)
    }
    fn full_height(self) -> Resized<Self> {
        Resized::new(SizeConstraint::Free, SizeConstraint::Full, self)
    }
}

impl<V: View> Resizable for V {}
