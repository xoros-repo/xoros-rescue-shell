//! The menu bar in the top row (`Esc` opens it, UI-2) and its menu trees.

use std::rc::Rc;

use super::printer::width;
use super::{Callback, Event, Key, Printer, Ui, Vec2};

pub enum MenuItem {
    Leaf(String, Callback),
    Subtree(String, Tree),
    Delimiter,
}

impl MenuItem {
    fn label(&self) -> &str {
        match self {
            MenuItem::Leaf(l, _) | MenuItem::Subtree(l, _) => l,
            MenuItem::Delimiter => "",
        }
    }

    fn selectable(&self) -> bool {
        !matches!(self, MenuItem::Delimiter)
    }
}

/// One menu: leaves (actions), submenus and delimiter lines.
#[derive(Default)]
pub struct Tree {
    pub items: Vec<MenuItem>,
}

impl Tree {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn leaf<F>(mut self, label: impl Into<String>, f: F) -> Self
    where
        F: Fn(&mut Ui) + 'static,
    {
        self.add_leaf(label, f);
        self
    }

    pub fn add_leaf<F>(&mut self, label: impl Into<String>, f: F)
    where
        F: Fn(&mut Ui) + 'static,
    {
        self.items.push(MenuItem::Leaf(label.into(), Rc::new(f)));
    }

    pub fn subtree(mut self, label: impl Into<String>, tree: Tree) -> Self {
        self.items.push(MenuItem::Subtree(label.into(), tree));
        self
    }

    pub fn delimiter(mut self) -> Self {
        self.items.push(MenuItem::Delimiter);
        self
    }

    fn first(&self) -> usize {
        self.items
            .iter()
            .position(MenuItem::selectable)
            .unwrap_or(0)
    }

    /// Next selectable row after `from` (wrapping), backwards when `!forward`.
    fn step(&self, from: usize, forward: bool) -> usize {
        let n = self.items.len();
        if n == 0 {
            return 0;
        }
        let mut i = from;
        for _ in 0..n {
            i = if forward {
                (i + 1) % n
            } else {
                (i + n - 1) % n
            };
            if self.items[i].selectable() {
                return i;
            }
        }
        from
    }

    fn box_width(&self) -> usize {
        self.items
            .iter()
            .map(|i| {
                width(i.label())
                    + match i {
                        MenuItem::Subtree(..) => 2,
                        _ => 0,
                    }
            })
            .max()
            .unwrap_or(0)
            + 4
    }
}

/// The bar with the top-level menus. While active it gets all key events.
#[derive(Default)]
pub struct MenuBar {
    menus: Vec<(String, Tree)>,
    active: bool,
    selected: usize,
    /// Selected row of the open menu and of each open submenu.
    path: Vec<usize>,
}

impl MenuBar {
    pub fn clear(&mut self) {
        self.menus.clear();
        self.active = false;
        self.path.clear();
    }

    pub fn add_subtree(&mut self, label: impl Into<String>, tree: Tree) -> &mut Self {
        self.menus.push((label.into(), tree));
        self
    }

    /// All leaves with their path ("Boot / Boot control"), for tests.
    pub fn leaves(&self) -> Vec<(String, Callback)> {
        fn walk(prefix: &str, tree: &Tree, out: &mut Vec<(String, Callback)>) {
            for item in &tree.items {
                match item {
                    MenuItem::Leaf(l, cb) => out.push((format!("{prefix} / {l}"), Rc::clone(cb))),
                    MenuItem::Subtree(l, t) => walk(&format!("{prefix} / {l}"), t, out),
                    MenuItem::Delimiter => {}
                }
            }
        }
        let mut out = Vec::new();
        for (label, tree) in &self.menus {
            walk(label, tree, &mut out);
        }
        out
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Open the first menu.
    pub fn activate(&mut self) {
        if self.menus.is_empty() {
            return;
        }
        self.active = true;
        self.open(0);
    }

    fn open(&mut self, menu: usize) {
        self.selected = menu;
        self.path = vec![self.menus[menu].1.first()];
    }

    /// The trees along the open path (top menu first).
    fn trees(&self) -> Vec<&Tree> {
        let mut out = vec![&self.menus[self.selected].1];
        for &row in &self.path[..self.path.len().saturating_sub(1)] {
            match out.last().and_then(|t| t.items.get(row)) {
                Some(MenuItem::Subtree(_, t)) => out.push(t),
                _ => break,
            }
        }
        out
    }

    /// Handle a key; returns the callback of a chosen leaf.
    pub fn on_event(&mut self, e: &Event) -> Option<Callback> {
        if self.menus.is_empty() {
            self.active = false;
            return None;
        }
        let n = self.menus.len();
        let depth = self.path.len();
        let (up, down, sub_first, leaf) = {
            let trees = self.trees();
            let tree = trees.last()?;
            let row = *self.path.last()?;
            let (sub_first, leaf) = match tree.items.get(row) {
                Some(MenuItem::Subtree(_, sub)) => (Some(sub.first()), None),
                Some(MenuItem::Leaf(_, cb)) => (None, Some(Rc::clone(cb))),
                _ => (None, None),
            };
            (tree.step(row, false), tree.step(row, true), sub_first, leaf)
        };
        match e {
            Event::Key(Key::Esc) => {
                if depth > 1 {
                    self.path.pop();
                } else {
                    self.active = false;
                }
            }
            Event::Key(Key::Left) if depth > 1 => {
                self.path.pop();
            }
            Event::Key(Key::Left) => self.open((self.selected + n - 1) % n),
            Event::Key(Key::Up) => *self.path.last_mut()? = up,
            Event::Key(Key::Down) => *self.path.last_mut()? = down,
            Event::Key(Key::Right) | Event::Key(Key::Enter) => {
                if let Some(first) = sub_first {
                    self.path.push(first);
                } else if let (Some(cb), true) = (leaf, e.is_enter()) {
                    self.active = false;
                    return Some(cb);
                } else if *e == Event::Key(Key::Right) {
                    self.open((self.selected + 1) % n);
                }
            }
            _ => {}
        }
        None
    }

    /// Start column and width of each title in the bar.
    fn titles(&self) -> Vec<(usize, usize)> {
        let mut x = 1;
        self.menus
            .iter()
            .map(|(label, _)| {
                let w = width(label) + 2;
                let at = x;
                x += w + 1;
                (at, w)
            })
            .collect()
    }

    /// Position and size of the open menu and of each open submenu, on a `screen`.
    fn boxes(&self, screen: Vec2) -> Vec<(Vec2, Vec2)> {
        let mut out = Vec::new();
        if !self.active || self.menus.is_empty() {
            return out;
        }
        let mut bx = self.titles().get(self.selected).map_or(0, |t| t.0);
        let mut by = 1;
        for (depth, tree) in self.trees().into_iter().enumerate() {
            let size = Vec2::new(tree.box_width(), tree.items.len() + 2);
            let at = Vec2::new(
                bx.min(screen.x.saturating_sub(size.x)),
                by.min(screen.y.saturating_sub(size.y)),
            );
            out.push((at, size));
            let sel = self.path.get(depth).copied().unwrap_or(0);
            bx = at.x + size.x - 1;
            by = at.y + 1 + sel;
        }
        out
    }

    /// A click in the bar row: open the menu under `x` (or close it when it is open).
    pub fn click_title(&mut self, x: usize) {
        let hit = self
            .titles()
            .iter()
            .position(|&(at, w)| (at..at + w).contains(&x));
        match hit {
            Some(i) if self.active && i == self.selected => self.active = false,
            Some(i) => {
                self.active = true;
                self.open(i);
            }
            None => self.active = false,
        }
    }

    /// A click while a menu is open: a title switches menus, an entry is chosen (the
    /// callback of a leaf is returned), anything else closes the menus.
    pub fn on_click(&mut self, pos: Vec2, screen: Vec2) -> Option<Callback> {
        if pos.y == 0 {
            self.click_title(pos.x);
            return None;
        }
        // Deepest box first: submenus are drawn over their parents.
        let boxes = self.boxes(screen);
        let hit = boxes.iter().enumerate().rev().find(|(_, (at, size))| {
            pos.x >= at.x && pos.x < at.x + size.x && pos.y > at.y && pos.y + 1 < at.y + size.y
        });
        let Some((depth, (at, _))) = hit else {
            self.active = false;
            return None;
        };
        let row = pos.y - at.y - 1;
        let tree = *self.trees().get(depth)?;
        let (sub_first, leaf) = match tree.items.get(row)? {
            MenuItem::Subtree(_, sub) => (Some(sub.first()), None),
            MenuItem::Leaf(_, cb) => (None, Some(Rc::clone(cb))),
            MenuItem::Delimiter => return None,
        };
        self.path.truncate(depth + 1);
        self.path[depth] = row;
        if let Some(first) = sub_first {
            self.path.push(first);
        }
        if leaf.is_some() {
            self.active = false;
        }
        leaf
    }

    pub fn draw(&self, p: &Printer) {
        let bar = p.sub(Vec2::new(0, 0), Vec2::new(p.size.x, 1));
        bar.with_style(p.theme.primary(), |b| b.fill());
        for (i, ((label, _), (x, _))) in self.menus.iter().zip(self.titles()).enumerate() {
            let text = format!(" {label} ");
            if self.active && i == self.selected {
                bar.with_style(p.theme.highlight(), |b| b.print((x, 0), &text));
            } else {
                bar.print((x, 0), &text);
            }
        }
        let trees = if self.active {
            self.trees()
        } else {
            Vec::new()
        };
        for (depth, (tree, (at, _))) in trees.into_iter().zip(self.boxes(p.size)).enumerate() {
            let sel = self.path.get(depth).copied().unwrap_or(0);
            draw_menu_box(p, at, tree, sel);
        }
    }
}

fn draw_menu_box(p: &Printer, pos: Vec2, tree: &Tree, selected: usize) {
    let w = tree.box_width();
    let size = Vec2::new(w, tree.items.len() + 2);
    let b = p.sub(pos, size);
    b.with_style(p.theme.primary(), |b| {
        b.fill();
        b.print((0, 0), "┌");
        b.print_hline((1, 0), w - 2, "─");
        b.print((w - 1, 0), "┐");
        for (i, item) in tree.items.iter().enumerate() {
            let y = i + 1;
            match item {
                MenuItem::Delimiter => {
                    b.print((0, y), "├");
                    b.print_hline((1, y), w - 2, "─");
                    b.print((w - 1, y), "┤");
                }
                _ => {
                    b.print((0, y), "│");
                    b.print((w - 1, y), "│");
                    let arrow = if matches!(item, MenuItem::Subtree(..)) {
                        " >"
                    } else {
                        ""
                    };
                    let inner = w - 4;
                    let text = format!(
                        " {:<pad$} ",
                        format!("{}{arrow}", item.label()),
                        pad = inner
                    );
                    if i == selected {
                        b.with_style(p.theme.highlight(), |b| b.print((1, y), &text));
                    } else {
                        b.print((1, y), &text);
                    }
                }
            }
        }
        let y = size.y - 1;
        b.print((0, y), "└");
        b.print_hline((1, y), w - 2, "─");
        b.print((w - 1, y), "┘");
    });
}
