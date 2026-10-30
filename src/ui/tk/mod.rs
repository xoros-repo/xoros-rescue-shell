//! A small retained-mode widget layer on top of ratatui (UI-1): a stack of layers (the
//! fullscreen Status screen, dialogs above it), a menu bar, views looked up by name and
//! callbacks that run on the UI thread with `&mut Ui`. Only what the screens of `crate::ui`
//! need is implemented.
//!
//! Background threads hand work to the UI thread through [`Ui::cb_sink`]. The event loop
//! redraws only after an event or a callback (the UI ticker sends one every 300 ms).

pub mod event;
pub mod menu;
pub mod printer;
pub mod traits;
pub mod views;

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::io;
use std::rc::Rc;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{
    poll, read, DisableMouseCapture, EnableMouseCapture, Event as CtEvent, KeyEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::{Frame, Terminal};

use crate::app::App;

pub use event::{Event, Key, MouseKind};
pub use printer::Printer;
pub use traits::View;

/// A callback run on the UI thread (button, menu leaf, key binding).
pub type Callback = Rc<dyn Fn(&mut Ui)>;

/// Work for the UI thread, sent from another thread.
pub type UiTask = Box<dyn FnOnce(&mut Ui) + Send>;

/// Sends work from any thread to the UI thread.
pub type CbSink = mpsc::Sender<UiTask>;

/// What a view did with an event.
pub enum EventResult {
    /// Not handled; the parent may use it (focus moves, global keys).
    Ignored,
    /// Handled; the callback (if any) runs once the event has been dispatched.
    Consumed(Option<Callback>),
}

impl EventResult {
    pub fn consumed() -> Self {
        EventResult::Consumed(None)
    }

    pub fn with_cb(cb: Callback) -> Self {
        EventResult::Consumed(Some(cb))
    }

    pub fn is_consumed(&self) -> bool {
        matches!(self, EventResult::Consumed(_))
    }
}

/// Where focus enters a view: from the front (first field) or the back (last field).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Front,
    Back,
}

/// A size or position in cells.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Vec2 {
    pub x: usize,
    pub y: usize,
}

impl Vec2 {
    pub const fn new(x: usize, y: usize) -> Self {
        Vec2 { x, y }
    }

    /// Component-wise minimum.
    pub fn min(self, o: Vec2) -> Vec2 {
        Vec2::new(self.x.min(o.x), self.y.min(o.y))
    }

    pub fn saturating_sub(self, o: Vec2) -> Vec2 {
        Vec2::new(self.x.saturating_sub(o.x), self.y.saturating_sub(o.y))
    }
}

impl From<(usize, usize)> for Vec2 {
    fn from((x, y): (usize, usize)) -> Self {
        Vec2::new(x, y)
    }
}

/// Colors of the UI. Only the 8 basic ANSI colors work on the Linux VT (RT-3); ratatui's
/// `Color::Gray` is ANSI white (37/47), `Color::White` would be the bright variant.
#[derive(Clone, Debug)]
pub struct Theme {
    /// Behind all views.
    pub background: Color,
    /// Background of screens, dialogs and the menu bar.
    pub view: Color,
    /// Normal text and borders.
    pub primary: Color,
    /// Input fields that do not have the focus.
    pub secondary: Color,
    /// Dialog titles.
    pub title: Color,
    /// Background of the focused item.
    pub highlight: Color,
    /// Background of the selected item of a list without the focus.
    pub highlight_inactive: Color,
    /// Text on a highlight.
    pub highlight_text: Color,
}

impl Theme {
    pub fn background(&self) -> Style {
        Style::default().bg(self.background)
    }

    pub fn primary(&self) -> Style {
        Style::default().fg(self.primary).bg(self.view)
    }

    pub fn secondary(&self) -> Style {
        Style::default().fg(self.secondary).bg(self.view)
    }

    pub fn title(&self) -> Style {
        Style::default().fg(self.title).bg(self.view)
    }

    pub fn highlight(&self) -> Style {
        Style::default().fg(self.highlight_text).bg(self.highlight)
    }

    pub fn highlight_inactive(&self) -> Style {
        Style::default()
            .fg(self.highlight_text)
            .bg(self.highlight_inactive)
    }
}

struct Layer {
    view: Box<dyn View>,
    fullscreen: bool,
    /// Position and size at the last redraw, for mouse events.
    area: Option<(Vec2, Vec2)>,
    /// Theme shown while this layer is open (preview in the settings screen).
    theme: Option<Theme>,
}

/// The UI: layers, menu bar, theme and the shared application state.
pub struct Ui {
    app: Arc<App>,
    theme: Theme,
    layers: Vec<Layer>,
    menubar: menu::MenuBar,
    global: Vec<(Event, Callback)>,
    pre: Vec<(Event, Callback)>,
    menu_builder: Option<Callback>,
    tx: CbSink,
    rx: mpsc::Receiver<UiTask>,
    size: Vec2,
    quit: bool,
    clear_requested: bool,
}

impl Ui {
    pub fn new(app: Arc<App>, theme: Theme) -> Self {
        let (tx, rx) = mpsc::channel();
        Ui {
            app,
            theme,
            layers: Vec::new(),
            menubar: menu::MenuBar::default(),
            global: Vec::new(),
            pre: Vec::new(),
            menu_builder: None,
            tx,
            rx,
            size: Vec2::new(80, 24),
            quit: false,
            clear_requested: false,
        }
    }

    /// The shared application state.
    pub fn app(&self) -> Arc<App> {
        Arc::clone(&self.app)
    }

    /// Use `theme` from now on (also ends a preview).
    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
        for l in &mut self.layers {
            l.theme = None;
        }
    }

    /// Show `theme` while the top layer is open; closing that layer goes back to the theme
    /// of [`Ui::set_theme`].
    pub fn preview_theme(&mut self, theme: Theme) {
        if let Some(l) = self.layers.last_mut() {
            l.theme = Some(theme);
        }
    }

    /// The theme in use: the preview of the topmost layer that has one, else the set theme.
    pub fn theme(&self) -> &Theme {
        self.layers
            .iter()
            .rev()
            .find_map(|l| l.theme.as_ref())
            .unwrap_or(&self.theme)
    }

    /// Size of the terminal at the last redraw.
    pub fn screen_size(&self) -> Vec2 {
        self.size
    }

    /// Sender for callbacks from other threads.
    pub fn cb_sink(&self) -> &CbSink {
        &self.tx
    }

    /// Leave the event loop (the app exits; systemd restarts it, RT-1).
    pub fn quit(&mut self) {
        self.quit = true;
    }

    /// Repaint the whole terminal on the next redraw (after a subshell or a rotation change).
    pub fn clear(&mut self) {
        self.clear_requested = true;
    }

    pub fn menubar(&mut self) -> &mut menu::MenuBar {
        &mut self.menubar
    }

    /// Open the menu bar on its first menu.
    pub fn select_menubar(&mut self) {
        self.menubar.activate();
    }

    /// `f` (re)builds the menus before the menu bar opens by a mouse click (Esc has its own
    /// global callback).
    pub fn set_menu_builder<F>(&mut self, f: F)
    where
        F: Fn(&mut Ui) + 'static,
    {
        self.menu_builder = Some(Rc::new(f));
    }

    /// Run `f` for `event` when no view used it.
    pub fn add_global_callback<F>(&mut self, event: impl Into<Event>, f: F)
    where
        F: Fn(&mut Ui) + 'static,
    {
        self.global.push((event.into(), Rc::new(f)));
    }

    /// Run `f` for `event` before any view sees it (the event is then dropped).
    pub fn set_on_pre_event<F>(&mut self, event: impl Into<Event>, f: F)
    where
        F: Fn(&mut Ui) + 'static,
    {
        let event = event.into();
        self.pre.retain(|(e, _)| *e != event);
        self.pre.push((event, Rc::new(f)));
    }

    /// A layer that fills the screen below the menu bar.
    pub fn add_fullscreen_layer<V: View>(&mut self, view: V) {
        self.push_layer(Box::new(view), true);
    }

    /// A layer centered on the screen.
    pub fn add_layer<V: View>(&mut self, view: V) {
        self.push_layer(Box::new(view), false);
    }

    fn push_layer(&mut self, mut view: Box<dyn View>, fullscreen: bool) {
        view.take_focus(Direction::Front);
        self.layers.push(Layer {
            view,
            fullscreen,
            area: None,
            theme: None,
        });
    }

    /// Remove the top layer.
    pub fn pop_layer(&mut self) -> Option<Box<dyn View>> {
        self.layers.pop().map(|l| l.view)
    }

    /// Number of layers.
    pub fn screen_len(&self) -> usize {
        self.layers.len()
    }

    /// Index of the topmost layer that contains a view named `name`.
    pub fn find_layer_from_name(&mut self, name: &str) -> Option<usize> {
        (0..self.layers.len()).rev().find(|&i| {
            let mut found = false;
            self.layers[i].view.call_on_any(name, &mut |_| found = true);
            found
        })
    }

    pub fn remove_layer(&mut self, index: usize) {
        if index < self.layers.len() {
            self.layers.remove(index);
        }
    }

    /// Run `f` on the view named `name` (searched from the top layer down) if it has type `V`.
    pub fn call_on_name<V: View, R>(
        &mut self,
        name: &str,
        f: impl FnOnce(&mut V) -> R,
    ) -> Option<R> {
        let mut f = Some(f);
        let mut out = None;
        for layer in self.layers.iter_mut().rev() {
            layer.view.call_on_any(name, &mut |any: &mut dyn Any| {
                if let Some(v) = any.downcast_mut::<V>() {
                    if let Some(f) = f.take() {
                        out = Some(f(v));
                    }
                }
            });
            if out.is_some() {
                break;
            }
        }
        out
    }

    /// Run the event loop on the terminal until [`Ui::quit`].
    pub fn run(&mut self) -> io::Result<()> {
        enable_raw_mode()?;
        let mut out = io::stdout();
        execute!(out, EnterAlternateScreen, EnableMouseCapture)?;
        let result = Terminal::new(CrosstermBackend::new(io::stdout()))
            .and_then(|mut t| self.event_loop(&mut t));
        let _ = execute!(
            io::stdout(),
            DisableMouseCapture,
            LeaveAlternateScreen,
            crossterm::cursor::Show
        );
        let _ = disable_raw_mode();
        result
    }

    fn event_loop(&mut self, term: &mut Terminal<CrosstermBackend<io::Stdout>>) -> io::Result<()> {
        term.hide_cursor()?;
        let mut dirty = true;
        while !self.quit {
            while let Ok(cb) = self.rx.try_recv() {
                cb(self);
                dirty = true;
            }
            if self.quit {
                break;
            }
            if self.clear_requested {
                self.clear_requested = false;
                // Not `Terminal::clear`: it asks the terminal for the cursor position and
                // fails when no answer comes. A new terminal object has an empty previous
                // buffer, so the next draw repaints every cell.
                execute!(io::stdout(), Clear(ClearType::All))?;
                *term = Terminal::new(CrosstermBackend::new(io::stdout()))?;
                term.hide_cursor()?;
                dirty = true;
            }
            if dirty {
                term.draw(|f| self.draw(f))?;
                dirty = false;
            }
            if poll(Duration::from_millis(50))? {
                match read()? {
                    CtEvent::Key(k) if k.kind != KeyEventKind::Release => {
                        self.on_event(Event::from_key(&k));
                        dirty = true;
                    }
                    CtEvent::Paste(text) => {
                        for c in text.chars().filter(|c| !c.is_control()) {
                            self.on_event(Event::Char(c));
                        }
                        dirty = true;
                    }
                    // Moves and releases are dropped without a redraw.
                    CtEvent::Mouse(m) => {
                        if let Some(e) = Event::from_mouse(&m) {
                            self.on_event(e);
                            dirty = true;
                        }
                    }
                    // Resize (rotation, RT-4) and anything else: redraw.
                    _ => dirty = true,
                }
            }
        }
        Ok(())
    }

    /// Dispatch one event: pre-event hooks, menu bar, top layer, global callbacks. Mouse
    /// positions are absolute here.
    pub fn on_event(&mut self, event: Event) {
        if event.mouse().is_some() {
            self.on_mouse(&event);
            return;
        }
        if let Some(cb) = find_cb(&self.pre, &event) {
            cb(self);
            return;
        }
        if self.menubar.is_active() {
            if let Some(cb) = self.menubar.on_event(&event) {
                cb(self);
            }
            return;
        }
        let r = match self.layers.last_mut() {
            Some(l) => l.view.on_event(&event),
            None => EventResult::Ignored,
        };
        match r {
            EventResult::Consumed(Some(cb)) => cb(self),
            EventResult::Consumed(None) => {}
            EventResult::Ignored => {
                if let Some(cb) = find_cb(&self.global, &event) {
                    cb(self);
                }
            }
        }
    }

    /// Mouse: the menu bar (row 0 and open menus), else the top layer only (dialogs are
    /// modal; clicks next to them are ignored).
    fn on_mouse(&mut self, event: &Event) {
        let Some((pos, kind)) = event.mouse() else {
            return;
        };
        if self.menubar.is_active() {
            if kind == MouseKind::Press {
                if let Some(cb) = self.menubar.on_click(pos, self.size) {
                    cb(self);
                }
            }
            return;
        }
        if pos.y == 0 {
            if kind == MouseKind::Press {
                if let Some(build) = self.menu_builder.clone() {
                    build(self);
                }
                self.menubar.click_title(pos.x);
            }
            return;
        }
        let Some(layer) = self.layers.last_mut() else {
            return;
        };
        let Some(rel) = layer.area.and_then(|(at, size)| event.mouse_in(at, size)) else {
            return;
        };
        if let EventResult::Consumed(Some(cb)) = layer.view.on_event(&rel) {
            cb(self);
        }
    }

    /// Run the callbacks sent through [`Ui::cb_sink`] so far.
    pub fn run_pending(&mut self) {
        while let Ok(cb) = self.rx.try_recv() {
            cb(self);
        }
    }

    /// Draw one frame on `term` (tests use ratatui's `TestBackend`).
    pub fn render<B: ratatui::backend::Backend>(
        &mut self,
        term: &mut Terminal<B>,
    ) -> Result<(), B::Error> {
        term.draw(|f| self.draw(f)).map(|_| ())
    }

    fn draw(&mut self, f: &mut Frame) {
        let area = f.area();
        self.size = Vec2::new(area.width.into(), area.height.into());
        let buf = RefCell::new(std::mem::replace(f.buffer_mut(), Buffer::empty(Rect::ZERO)));
        let cursor = Cell::new(None);
        let theme = self.theme().clone();
        {
            let root = Printer::new(&buf, &theme, &cursor, area);
            root.with_style(theme.background(), |p| p.fill());
            self.draw_layers(&root);
            self.menubar.draw(&root);
        }
        *f.buffer_mut() = buf.into_inner();
        if let Some(pos) = cursor.get() {
            f.set_cursor_position(pos);
        }
    }

    fn draw_layers(&mut self, root: &Printer) {
        let screen = self.size;
        // Row 0 is the menu bar; the last row belongs to the fullscreen layer (status line).
        let below_bar = Vec2::new(screen.x, screen.y.saturating_sub(1));
        let dialog_area = Vec2::new(screen.x, screen.y.saturating_sub(2));
        let n = self.layers.len();
        let menu_open = self.menubar.is_active();
        for (i, layer) in self.layers.iter_mut().enumerate() {
            let focused = i + 1 == n && !menu_open;
            let (pos, size) = if layer.fullscreen {
                (Vec2::new(0, 1), below_bar)
            } else {
                let size = layer.view.required_size(dialog_area).min(dialog_area);
                let x = (dialog_area.x - size.x) / 2;
                let y = 1 + (dialog_area.y - size.y) / 2;
                (Vec2::new(x, y), size)
            };
            layer.view.layout(size);
            layer.area = Some((pos, size));
            let p = root.sub(pos, size).focused(focused);
            p.with_style(root.theme.primary(), |p| p.fill());
            layer.view.draw(&p);
        }
    }
}

fn find_cb(list: &[(Event, Callback)], event: &Event) -> Option<Callback> {
    list.iter()
        .find(|(e, _)| e == event)
        .map(|(_, cb)| Rc::clone(cb))
}
