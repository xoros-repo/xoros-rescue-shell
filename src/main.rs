use cursive::{event::Key, menu, traits::*, views::Dialog};
use std::sync::atomic::{AtomicUsize, Ordering};
use cursive::theme::{BaseColor, Color, Palette, PaletteColor, Theme};

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

fn main() {
    let mut c = cursive::termion();
    let mut theme = Theme::default();
    let mut palette = Palette::default();
    const VERSION: &str = env!("CARGO_PKG_VERSION");

    palette[PaletteColor::Background] = Color::Light(BaseColor::Black);
    palette[PaletteColor::View] = Color::Light(BaseColor::Yellow);
    palette[PaletteColor::Highlight] = Color::Dark(BaseColor::Black);
    palette[PaletteColor::HighlightText] = Color::Light(BaseColor::White);
    theme.palette = palette;
    c.set_theme(theme);

    // We'll use a counter to name new files.
    let counter = AtomicUsize::new(1);

    c.menubar()
        .add_subtree(
            "File",
            menu::Tree::new()
                .leaf("New", move |s| {
                    let i = counter.fetch_add(1, Ordering::Relaxed);
                    let filename = format!("New {i}");
                    s.menubar()
                        .find_subtree("File")
                        .unwrap()
                        .find_subtree("Recent")
                        .unwrap()
                        .insert_leaf(0, filename, |_| ());

                    s.add_layer(Dialog::info("New file!"));
                })
                // ... and of sub-trees, which open up when selected.
                .subtree(
                    "Recent",
                    // The `.with()` method can help when running loops
                    // within builder patterns.
                    menu::Tree::new().with(|tree| {
                        for i in 1..100 {
                            // We don't actually do anything here,
                            // but you could!
                            tree.add_item(menu::Item::leaf(format!("Item {i}"), |_| ()).with(|s| {
                                if i % 5 == 0 { s.disable(); }
                            }))
                        }
                    }),
                )
                // Delimiter are simple lines between items,
                // and cannot be selected.
                .delimiter()
                .with(|tree| {
                    for i in 1..10 {
                        tree.add_leaf(format!("Option {i}"), |_| ());
                    }
                }),
        )
        .add_subtree(
            "Help",
            menu::Tree::new()
                .subtree(
                    "Help",
                    menu::Tree::new()
                        .leaf("General", |s| {
                            s.add_layer(Dialog::info("Help message!"))
                        })
                        .leaf("Online", |s| {
                            let text = "Google it yourself!\n\
                                        Kids, these days...";
                            s.add_layer(Dialog::info(text))
                        }),
                )
                .leaf("About", |s| {
                    s.add_layer(Dialog::info(format!("Xoros Rescue Shell v{}", VERSION)))
                }),
        )
        .add_delimiter()
        .add_leaf("Quit", |s| s.quit());

    // When `autohide` is on (default), the menu only appears when active.
    // Turning it off will leave the menu always visible.
    // Try uncommenting this line!

//    siv.set_autohide_menu(false);

    c.add_global_callback(Key::Esc, |s| s.select_menubar());

    c.add_layer(Dialog::text("Press ESC to continue..."));
    c.status_bar(format!("Xoros Rescue Shell v{}", VERSION));
    c.set_fps(30);
    c.run();
}
