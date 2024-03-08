use cursive::traits::{Nameable, Resizable};
use cursive::View;

pub trait StatusBarExt {
    fn status_bar(&mut self, content: impl Into<cursive::utils::span::SpannedString<cursive::theme::Style>>) -> cursive::views::TextContent;
    fn get_status_bar_content(&mut self) -> cursive::views::TextContentRef;
    fn set_status_bar_content(&mut self, content: impl Into<cursive::utils::span::SpannedString<cursive::theme::Style>>);
}

impl StatusBarExt for cursive::Cursive {
    /// Create a new status bar, set to the given content.
    fn status_bar(&mut self, content: impl Into<cursive::utils::span::SpannedString<cursive::theme::Style>>) -> cursive::views::TextContent {
        let text_content = cursive::views::TextContent::new(content);
        self.screen_mut()
            .add_transparent_layer(
                cursive::views::OnLayoutView::new(
                    cursive::views::FixedLayout::new().child(
                        cursive::Rect::from_point(cursive::Vec2::zero()),
                        cursive::views::Layer::new(
                            cursive::views::TextView::new_with_content(text_content.clone()).with_name("status"),
                        )
                            .full_width(),
                    ),
                    |layout, size| {
                        let rect = cursive::Rect::from_size((0, size.y - 1), (size.x, 1));
                        layout.set_child_position(0, rect);
                        layout.layout(size);
                    },
                )
                    .full_screen(),
            );
        text_content
    }

    fn get_status_bar_content(&mut self) -> cursive::views::TextContentRef {
        self.call_on_name("status", |text_view: &mut cursive::views::TextView| {
            text_view.get_content()
        })
            .expect("get_status")
    }

    fn set_status_bar_content(&mut self, content: impl Into<cursive::utils::span::SpannedString<cursive::theme::Style>>) {
        self.call_on_name("status", |text_view: &mut cursive::views::TextView| {
            text_view.set_content(content);
        })
            .expect("set_status")
    }
}
