//! The views used by the screens: text, scrolling, layouts, form fields, lists, buttons,
//! dialogs, panels, names and size limits.

use std::any::Any;
use std::cell::Cell;
use std::rc::Rc;

use super::printer::{width, wrap};
use super::{Callback, Direction, Event, EventResult, Key, MouseKind, Printer, Ui, Vec2, View};

/// Callbacks of input fields and lists.
type OnText = Rc<dyn Fn(&mut Ui, &str)>;
type OnToggle = Rc<dyn Fn(&mut Ui, bool)>;
type OnPick<T> = Rc<dyn Fn(&mut Ui, &T)>;

/// "Unlimited" height for content inside a scroll view.
const BIG: usize = 100_000;

/// A left click (anywhere in the view that gets it).
fn is_press(e: &Event) -> bool {
    matches!(e.mouse(), Some((_, MouseKind::Press)))
}

fn is_scroll_key(e: &Event) -> bool {
    matches!(
        e,
        Event::Key(Key::Up | Key::Down | Key::PageUp | Key::PageDown | Key::Home | Key::End)
    )
}

fn draw_border(p: &Printer, title: &str) {
    let Vec2 { x: w, y: h } = p.size;
    if w < 2 || h < 2 {
        return;
    }
    p.print((0, 0), "┌");
    p.print_hline((1, 0), w - 2, "─");
    p.print((w - 1, 0), "┐");
    for y in 1..h - 1 {
        p.print((0, y), "│");
        p.print((w - 1, y), "│");
    }
    p.print((0, h - 1), "└");
    p.print_hline((1, h - 1), w - 2, "─");
    p.print((w - 1, h - 1), "┘");
    if !title.is_empty() && w > 6 {
        let t = crate::report::ellipsize(title, w - 6);
        let x = (w - width(&t) - 2) / 2;
        p.with_style(p.theme.title(), |p| p.print((x, 0), &format!(" {t} ")));
    }
}

// --------------------------------------------------------------------------------------------
// TextView

/// Text, wrapped at the view width (explicit line breaks are kept).
pub struct TextView {
    content: String,
    wrap: bool,
    /// Lines for the last width.
    cache: Option<(usize, Vec<String>, usize)>,
}

impl TextView {
    pub fn new(content: impl Into<String>) -> Self {
        TextView {
            content: content.into(),
            wrap: true,
            cache: None,
        }
    }

    /// Cut long lines instead of wrapping them.
    pub fn no_wrap(mut self) -> Self {
        self.wrap = false;
        self
    }

    pub fn set_content(&mut self, content: impl Into<String>) {
        self.content = content.into();
        self.cache = None;
    }

    pub fn get_content(&self) -> &str {
        &self.content
    }

    /// Lines and their maximum width for `w` columns.
    fn lines(&mut self, w: usize) -> (&[String], usize) {
        if self.cache.as_ref().is_none_or(|(cw, _, _)| *cw != w) {
            let lines = if self.wrap {
                wrap(&self.content, w)
            } else {
                let c = self.content.strip_suffix('\n').unwrap_or(&self.content);
                c.split('\n').map(str::to_string).collect()
            };
            let max = lines.iter().map(|l| width(l)).max().unwrap_or(0);
            self.cache = Some((w, lines, max));
        }
        let (_, lines, max) = self.cache.as_ref().expect("cache filled above");
        (lines, *max)
    }
}

impl View for TextView {
    fn draw(&self, p: &Printer) {
        if let Some((_, lines, _)) = &self.cache {
            for (y, line) in lines.iter().enumerate().take(p.size.y) {
                p.print((0, y), line);
            }
        }
    }

    fn required_size(&mut self, c: Vec2) -> Vec2 {
        let (lines, max) = self.lines(c.x);
        Vec2::new(max, lines.len())
    }

    fn layout(&mut self, size: Vec2) {
        self.lines(size.x);
    }
}

// --------------------------------------------------------------------------------------------
// ScrollView

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollStrategy {
    /// Keep the position (and the focused row visible).
    KeepRow,
    /// Show the end and follow new content until the user scrolls up (logs).
    StickToBottom,
}

/// Vertical scrolling with a scroll bar when the content is taller than the view.
pub struct ScrollView<V> {
    inner: V,
    offset: usize,
    content: Vec2,
    view: Vec2,
    bar: bool,
    stick: bool,
    /// Sticking to the bottom (no scrolling up since).
    follow: bool,
    /// The inner view's focused row at the last layout: it is scrolled into view only when
    /// it changes, so the mouse wheel can scroll away from it.
    last_important: Option<usize>,
}

impl<V: View> ScrollView<V> {
    pub fn new(inner: V) -> Self {
        ScrollView {
            inner,
            offset: 0,
            content: Vec2::default(),
            view: Vec2::default(),
            bar: false,
            stick: false,
            follow: false,
            last_important: None,
        }
    }

    pub fn scroll_strategy(mut self, s: ScrollStrategy) -> Self {
        self.stick = s == ScrollStrategy::StickToBottom;
        self.follow = self.stick;
        self
    }

    fn max_offset(&self) -> usize {
        self.content.y.saturating_sub(self.view.y)
    }

    fn scroll_to(&mut self, offset: usize) -> EventResult {
        let max = self.max_offset();
        let before = self.offset;
        self.offset = offset.min(max);
        self.follow = self.offset == max;
        if self.offset != before {
            EventResult::consumed()
        } else {
            EventResult::Ignored
        }
    }

    /// Wheel scrolls 3 rows, a click on the bar jumps there, other clicks go to the content.
    fn on_mouse(&mut self, pos: Vec2, kind: MouseKind) -> EventResult {
        if self.bar && pos.x + 1 == self.view.x && kind == MouseKind::Press {
            let h = self.view.y.saturating_sub(1).max(1);
            return self.scroll_to(pos.y * self.max_offset() / h);
        }
        let inner = Event::Mouse {
            pos: Vec2::new(pos.x, pos.y + self.offset),
            kind,
        };
        let r = self.inner.on_event(&inner);
        if r.is_consumed() {
            return r;
        }
        match kind {
            MouseKind::WheelUp => self.scroll_to(self.offset.saturating_sub(3)),
            MouseKind::WheelDown => self.scroll_to(self.offset + 3),
            MouseKind::Press => EventResult::Ignored,
        }
    }

    /// Inner size for `c`, and whether a scroll bar is needed.
    fn measure(&mut self, c: Vec2) -> (Vec2, bool) {
        let r = self.inner.required_size(Vec2::new(c.x, BIG));
        if r.y > c.y && c.x > 1 {
            (self.inner.required_size(Vec2::new(c.x - 1, BIG)), true)
        } else {
            (r, false)
        }
    }
}

impl<V: View> View for ScrollView<V> {
    fn draw(&self, p: &Printer) {
        let inner = p.scrolled(self.offset, self.content);
        self.inner.draw(&inner);
        if self.bar && self.view.y > 0 {
            let h = self.view.y;
            let thumb = (h * h / self.content.y.max(1)).clamp(1, h);
            let max = self.max_offset();
            let start = ((h - thumb) * self.offset).checked_div(max).unwrap_or(0);
            let x = self.view.x - 1;
            for y in 0..h {
                let s = if (start..start + thumb).contains(&y) {
                    "█"
                } else {
                    "│"
                };
                p.print((x, y), s);
            }
        }
    }

    fn required_size(&mut self, c: Vec2) -> Vec2 {
        let (r, bar) = self.measure(c);
        Vec2::new(r.x + usize::from(bar), r.y.min(c.y))
    }

    fn layout(&mut self, size: Vec2) {
        let (r, bar) = self.measure(size);
        self.bar = bar;
        self.view = size;
        self.content = Vec2::new(size.x - usize::from(bar), r.y.max(size.y));
        self.inner.layout(self.content);
        let important = self.inner.important_row();
        if let Some(row) = important.filter(|_| important != self.last_important) {
            if row < self.offset {
                self.offset = row;
            } else if row >= self.offset + size.y {
                self.offset = row + 1 - size.y;
            }
        }
        self.last_important = important;
        if self.stick && self.follow {
            self.offset = self.max_offset();
        }
        self.offset = self.offset.min(self.max_offset());
    }

    fn on_event(&mut self, e: &Event) -> EventResult {
        if let Some((pos, kind)) = e.mouse() {
            return self.on_mouse(pos, kind);
        }
        let r = self.inner.on_event(e);
        if r.is_consumed() {
            return r;
        }
        let before = self.offset;
        let max = self.max_offset();
        let page = self.view.y.saturating_sub(1).max(1);
        self.offset = match e {
            Event::Key(Key::Up) => before.saturating_sub(1),
            Event::Key(Key::Down) => (before + 1).min(max),
            Event::Key(Key::PageUp) => before.saturating_sub(page),
            Event::Key(Key::PageDown) => (before + page).min(max),
            Event::Key(Key::Home) => 0,
            Event::Key(Key::End) => max,
            _ => return EventResult::Ignored,
        };
        self.follow = self.offset == max;
        if self.offset != before {
            EventResult::consumed()
        } else {
            EventResult::Ignored
        }
    }

    fn take_focus(&mut self, dir: Direction) -> bool {
        self.inner.take_focus(dir)
    }

    fn call_on_any(&mut self, name: &str, f: &mut dyn FnMut(&mut dyn Any)) {
        self.inner.call_on_any(name, f)
    }
}

// --------------------------------------------------------------------------------------------
// Resized

/// A size limit for one axis.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SizeConstraint {
    /// What the view wants.
    Free,
    /// What the view wants, at most this.
    AtMost(usize),
    /// Exactly this (or what is available).
    Fixed(usize),
    /// Everything available.
    Full,
}

impl SizeConstraint {
    fn constraint(self, available: usize) -> usize {
        match self {
            SizeConstraint::Free | SizeConstraint::Full => available,
            SizeConstraint::AtMost(n) | SizeConstraint::Fixed(n) => n.min(available),
        }
    }

    fn result(self, wanted: usize, available: usize) -> usize {
        match self {
            SizeConstraint::Free => wanted.min(available),
            SizeConstraint::AtMost(n) => wanted.min(n).min(available),
            SizeConstraint::Fixed(n) => n.min(available),
            SizeConstraint::Full => available,
        }
    }
}

/// Size limits around a view (see [`super::traits::Resizable`]).
pub struct Resized<V> {
    w: SizeConstraint,
    h: SizeConstraint,
    inner: V,
}

impl<V: View> Resized<V> {
    pub fn new(w: SizeConstraint, h: SizeConstraint, inner: V) -> Self {
        Resized { w, h, inner }
    }
}

impl<V: View> View for Resized<V> {
    fn draw(&self, p: &Printer) {
        self.inner.draw(p)
    }

    fn required_size(&mut self, c: Vec2) -> Vec2 {
        let inner_c = Vec2::new(self.w.constraint(c.x), self.h.constraint(c.y));
        let r = self.inner.required_size(inner_c);
        Vec2::new(self.w.result(r.x, c.x), self.h.result(r.y, c.y))
    }

    fn layout(&mut self, size: Vec2) {
        self.inner.layout(size)
    }

    fn on_event(&mut self, e: &Event) -> EventResult {
        self.inner.on_event(e)
    }

    fn take_focus(&mut self, dir: Direction) -> bool {
        self.inner.take_focus(dir)
    }

    fn call_on_any(&mut self, name: &str, f: &mut dyn FnMut(&mut dyn Any)) {
        self.inner.call_on_any(name, f)
    }

    fn important_row(&self) -> Option<usize> {
        self.inner.important_row()
    }
}

// --------------------------------------------------------------------------------------------
// LinearLayout

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Orientation {
    Vertical,
    Horizontal,
}

/// Children above each other or side by side; arrows and Tab move the focus between them.
pub struct LinearLayout {
    orientation: Orientation,
    children: Vec<Box<dyn View>>,
    sizes: Vec<Vec2>,
    focus: Option<usize>,
}

impl LinearLayout {
    fn new(orientation: Orientation) -> Self {
        LinearLayout {
            orientation,
            children: Vec::new(),
            sizes: Vec::new(),
            focus: None,
        }
    }

    pub fn vertical() -> Self {
        Self::new(Orientation::Vertical)
    }

    pub fn horizontal() -> Self {
        Self::new(Orientation::Horizontal)
    }

    pub fn child<V: View>(mut self, v: V) -> Self {
        self.add_child(v);
        self
    }

    pub fn add_child<V: View>(&mut self, v: V) {
        self.children.push(Box::new(v));
    }

    fn main(&self, v: Vec2) -> usize {
        match self.orientation {
            Orientation::Vertical => v.y,
            Orientation::Horizontal => v.x,
        }
    }

    fn with_main(&self, v: Vec2, m: usize) -> Vec2 {
        match self.orientation {
            Orientation::Vertical => Vec2::new(v.x, m),
            Orientation::Horizontal => Vec2::new(m, v.y),
        }
    }

    /// Children sizes within `c`: when they do not fit, the biggest child gets what the
    /// others leave, then the children are cut in order.
    fn measure(&mut self, c: Vec2) -> Vec<Vec2> {
        let mut sizes: Vec<Vec2> = self
            .children
            .iter_mut()
            .map(|ch| ch.required_size(c))
            .collect();
        let limit = self.main(c);
        let total: usize = sizes.iter().map(|s| self.main(*s)).sum();
        if total > limit {
            if let Some(i) = (0..sizes.len()).max_by_key(|&i| self.main(sizes[i])) {
                let others = total - self.main(sizes[i]);
                let room = limit.saturating_sub(others);
                let cc = self.with_main(c, room);
                let r = self.children[i].required_size(cc);
                sizes[i] = self.with_main(r, self.main(r).min(room));
            }
            let mut left = limit;
            for s in sizes.iter_mut() {
                let m = self.main(*s).min(left);
                left -= m;
                *s = self.with_main(*s, m);
            }
        }
        sizes
    }

    fn move_focus(&mut self, from: usize, forward: bool) -> bool {
        let found = if forward {
            (from + 1..self.children.len()).find(|&j| self.children[j].take_focus(Direction::Front))
        } else {
            (0..from)
                .rev()
                .find(|&j| self.children[j].take_focus(Direction::Back))
        };
        if let Some(j) = found {
            self.focus = Some(j);
        }
        found.is_some()
    }

    fn offset_of(&self, i: usize) -> Vec2 {
        let m: usize = self.sizes.iter().take(i).map(|s| self.main(*s)).sum();
        self.with_main(Vec2::default(), m)
    }
}

impl View for LinearLayout {
    fn draw(&self, p: &Printer) {
        for (i, (child, size)) in self.children.iter().zip(&self.sizes).enumerate() {
            let sub = p
                .sub(self.offset_of(i), *size)
                .focused(self.focus == Some(i));
            child.draw(&sub);
        }
    }

    fn required_size(&mut self, c: Vec2) -> Vec2 {
        let sizes = self.measure(c);
        let main: usize = sizes.iter().map(|s| self.main(*s)).sum();
        let cross = sizes
            .iter()
            .map(|s| match self.orientation {
                Orientation::Vertical => s.x,
                Orientation::Horizontal => s.y,
            })
            .max()
            .unwrap_or(0);
        match self.orientation {
            Orientation::Vertical => Vec2::new(cross, main),
            Orientation::Horizontal => Vec2::new(main, cross),
        }
    }

    fn layout(&mut self, size: Vec2) {
        let sizes = self.measure(size);
        self.sizes = sizes
            .iter()
            .map(|s| match self.orientation {
                Orientation::Vertical => Vec2::new(size.x, s.y),
                Orientation::Horizontal => Vec2::new(s.x, size.y),
            })
            .collect();
        for (child, s) in self.children.iter_mut().zip(&self.sizes) {
            child.layout(*s);
        }
    }

    fn on_event(&mut self, e: &Event) -> EventResult {
        if let Some((_, kind)) = e.mouse() {
            // The child under the pointer; a click it uses gives it the focus.
            for i in 0..self.children.len() {
                let Some(rel) = e.mouse_in(self.offset_of(i), self.sizes[i]) else {
                    continue;
                };
                let r = self.children[i].on_event(&rel);
                if r.is_consumed() && kind == MouseKind::Press {
                    self.focus = Some(i);
                }
                return r;
            }
            return EventResult::Ignored;
        }
        let Some(i) = self.focus else {
            // Nothing focusable: scroll keys go to the first child that uses them.
            for child in &mut self.children {
                let r = child.on_event(e);
                if r.is_consumed() {
                    return r;
                }
            }
            return EventResult::Ignored;
        };
        let r = self.children[i].on_event(e);
        if r.is_consumed() {
            return r;
        }
        let vertical = self.orientation == Orientation::Vertical;
        let forward = match e {
            Event::Key(Key::Tab) => Some(true),
            Event::Shift(Key::Tab) => Some(false),
            Event::Key(Key::Down) if vertical => Some(true),
            Event::Key(Key::Up) if vertical => Some(false),
            Event::Key(Key::Right) if !vertical => Some(true),
            Event::Key(Key::Left) if !vertical => Some(false),
            _ => None,
        };
        if let Some(fwd) = forward {
            if self.move_focus(i, fwd) {
                return EventResult::consumed();
            }
        }
        // Page keys scroll a text next to the focused field (e.g. a list below a report).
        if matches!(e, Event::Key(Key::PageUp | Key::PageDown)) {
            for (j, child) in self.children.iter_mut().enumerate() {
                if j != i {
                    let r = child.on_event(e);
                    if r.is_consumed() {
                        return r;
                    }
                }
            }
        }
        EventResult::Ignored
    }

    fn take_focus(&mut self, dir: Direction) -> bool {
        let n = self.children.len();
        let found = match dir {
            Direction::Front => (0..n).find(|&j| self.children[j].take_focus(dir)),
            Direction::Back => (0..n).rev().find(|&j| self.children[j].take_focus(dir)),
        };
        if found.is_some() {
            self.focus = found;
        }
        found.is_some()
    }

    fn call_on_any(&mut self, name: &str, f: &mut dyn FnMut(&mut dyn Any)) {
        for child in &mut self.children {
            child.call_on_any(name, f);
        }
    }

    fn important_row(&self) -> Option<usize> {
        let i = self.focus?;
        let inner = self.children[i].important_row().unwrap_or(0);
        Some(match self.orientation {
            Orientation::Vertical => self.offset_of(i).y + inner,
            Orientation::Horizontal => inner,
        })
    }
}

// --------------------------------------------------------------------------------------------
// ListView

enum ListChild {
    Row(String, Box<dyn View>),
    Delimiter,
}

/// A form: labels on the left, fields on the right, optional blank delimiter rows.
#[derive(Default)]
pub struct ListView {
    children: Vec<ListChild>,
    heights: Vec<usize>,
    label_w: usize,
    focus: Option<usize>,
}

impl ListView {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_child<V: View>(&mut self, label: impl Into<String>, view: V) {
        self.children
            .push(ListChild::Row(label.into(), Box::new(view)));
    }

    pub fn add_delimiter(&mut self) {
        self.children.push(ListChild::Delimiter);
    }

    fn label_width(&self) -> usize {
        self.children
            .iter()
            .map(|c| match c {
                ListChild::Row(l, _) => width(l) + 1,
                ListChild::Delimiter => 0,
            })
            .max()
            .unwrap_or(0)
    }

    /// A click on a field goes to it; a click on a label only moves the focus to its field.
    fn on_mouse(&mut self, e: &Event, pos: Vec2, kind: MouseKind) -> EventResult {
        let mut y = 0;
        for (i, h) in self.heights.iter().enumerate() {
            if pos.y < y || pos.y >= y + h {
                y += h;
                continue;
            }
            let ListChild::Row(_, v) = &mut self.children[i] else {
                return EventResult::Ignored;
            };
            if pos.x < self.label_w {
                if kind == MouseKind::Press && v.take_focus(Direction::Front) {
                    self.focus = Some(i);
                    return EventResult::consumed();
                }
                return EventResult::Ignored;
            }
            let field = Vec2::new(self.label_w, y);
            let Some(rel) = e.mouse_in(field, Vec2::new(usize::MAX / 2, *h)) else {
                return EventResult::Ignored;
            };
            let r = v.on_event(&rel);
            if r.is_consumed() && kind == MouseKind::Press {
                self.focus = Some(i);
            }
            return r;
        }
        EventResult::Ignored
    }

    fn focus_from(&mut self, from: Option<usize>, forward: bool) -> Option<usize> {
        let n = self.children.len();
        let dir = if forward {
            Direction::Front
        } else {
            Direction::Back
        };
        let order: Vec<usize> = match (from, forward) {
            (Some(i), true) => (i + 1..n).collect(),
            (Some(i), false) => (0..i).rev().collect(),
            (None, true) => (0..n).collect(),
            (None, false) => (0..n).rev().collect(),
        };
        order.into_iter().find(|&j| match &mut self.children[j] {
            ListChild::Row(_, v) => v.take_focus(dir),
            ListChild::Delimiter => false,
        })
    }
}

impl View for ListView {
    fn draw(&self, p: &Printer) {
        let mut y = 0;
        for (i, (child, h)) in self.children.iter().zip(&self.heights).enumerate() {
            if let ListChild::Row(label, view) = child {
                p.print((0, y), label);
                let sub = p
                    .sub(
                        Vec2::new(self.label_w, y),
                        Vec2::new(p.size.x.saturating_sub(self.label_w), *h),
                    )
                    .focused(self.focus == Some(i));
                view.draw(&sub);
            }
            y += h;
        }
    }

    fn required_size(&mut self, c: Vec2) -> Vec2 {
        let lw = self.label_width();
        let inner = Vec2::new(c.x.saturating_sub(lw), c.y);
        let mut w = 0;
        let mut h = 0;
        for child in &mut self.children {
            match child {
                ListChild::Row(_, v) => {
                    let r = v.required_size(inner);
                    w = w.max(r.x);
                    h += r.y.max(1);
                }
                ListChild::Delimiter => h += 1,
            }
        }
        Vec2::new(lw + w, h)
    }

    fn layout(&mut self, size: Vec2) {
        self.label_w = self.label_width();
        let inner = Vec2::new(size.x.saturating_sub(self.label_w), size.y);
        self.heights = self
            .children
            .iter_mut()
            .map(|c| match c {
                ListChild::Row(_, v) => {
                    let h = v.required_size(inner).y.max(1);
                    v.layout(Vec2::new(inner.x, h));
                    h
                }
                ListChild::Delimiter => 1,
            })
            .collect();
    }

    fn on_event(&mut self, e: &Event) -> EventResult {
        if let Some((pos, kind)) = e.mouse() {
            return self.on_mouse(e, pos, kind);
        }
        let Some(i) = self.focus else {
            return EventResult::Ignored;
        };
        if let ListChild::Row(_, v) = &mut self.children[i] {
            let r = v.on_event(e);
            if r.is_consumed() {
                return r;
            }
        }
        let forward = match e {
            Event::Key(Key::Tab | Key::Down) => true,
            Event::Shift(Key::Tab) | Event::Key(Key::Up) => false,
            _ => return EventResult::Ignored,
        };
        match self.focus_from(Some(i), forward) {
            Some(j) => {
                self.focus = Some(j);
                EventResult::consumed()
            }
            None => EventResult::Ignored,
        }
    }

    fn take_focus(&mut self, dir: Direction) -> bool {
        let found = self.focus_from(None, dir == Direction::Front);
        if found.is_some() {
            self.focus = found;
        }
        found.is_some()
    }

    fn call_on_any(&mut self, name: &str, f: &mut dyn FnMut(&mut dyn Any)) {
        for child in &mut self.children {
            if let ListChild::Row(_, v) = child {
                v.call_on_any(name, f);
            }
        }
    }

    fn important_row(&self) -> Option<usize> {
        let i = self.focus?;
        Some(self.heights.iter().take(i).sum())
    }
}

// --------------------------------------------------------------------------------------------
// EditView

/// A one-line input field.
#[derive(Default)]
pub struct EditView {
    content: String,
    /// Cursor position in chars.
    cursor: usize,
    /// First visible char.
    offset: usize,
    secret: bool,
    max_len: Option<usize>,
    on_submit: Option<OnText>,
    width: usize,
}

impl EditView {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn content(mut self, content: impl Into<String>) -> Self {
        self.set_content(content);
        self
    }

    pub fn set_content(&mut self, content: impl Into<String>) {
        self.content = content.into();
        self.cursor = self.content.chars().count();
        self.offset = 0;
    }

    pub fn get_content(&self) -> String {
        self.content.clone()
    }

    /// Show `*` instead of the text (passphrases, PIN).
    pub fn secret(mut self) -> Self {
        self.secret = true;
        self
    }

    pub fn set_secret(&mut self, secret: bool) {
        self.secret = secret;
    }

    /// At most `n` characters.
    pub fn max_content_width(mut self, n: usize) -> Self {
        self.max_len = Some(n);
        self
    }

    /// Run `f` with the content on Enter.
    pub fn on_submit<F>(mut self, f: F) -> Self
    where
        F: Fn(&mut Ui, &str) + 'static,
    {
        self.on_submit = Some(Rc::new(f));
        self
    }

    fn byte_at(&self, char_idx: usize) -> usize {
        self.content
            .char_indices()
            .nth(char_idx)
            .map_or(self.content.len(), |(b, _)| b)
    }

    fn len(&self) -> usize {
        self.content.chars().count()
    }
}

impl View for EditView {
    fn draw(&self, p: &Printer) {
        let w = p.size.x;
        let shown: String = if self.secret {
            "*".repeat(self.len())
        } else {
            self.content.clone()
        };
        let visible: String = shown.chars().skip(self.offset).take(w).collect();
        let vw = visible.chars().count();
        let style = if p.focused {
            p.theme.highlight()
        } else {
            p.theme.secondary()
        };
        p.with_style(style, |p| {
            p.print((0, 0), &visible);
            p.print_hline((vw, 0), w.saturating_sub(vw), "_");
        });
        p.set_cursor((self.cursor.saturating_sub(self.offset), 0));
    }

    fn required_size(&mut self, c: Vec2) -> Vec2 {
        Vec2::new((self.len() + 1).max(10).min(c.x), 1)
    }

    fn layout(&mut self, size: Vec2) {
        self.width = size.x.max(1);
        if self.cursor < self.offset {
            self.offset = self.cursor;
        } else if self.cursor >= self.offset + self.width {
            self.offset = self.cursor + 1 - self.width;
        }
    }

    fn on_event(&mut self, e: &Event) -> EventResult {
        match e {
            // A click puts the cursor there.
            Event::Mouse {
                pos,
                kind: MouseKind::Press,
            } => self.cursor = (self.offset + pos.x).min(self.len()),
            Event::Char(c) => {
                if self.max_len.is_none_or(|m| self.len() < m) {
                    let b = self.byte_at(self.cursor);
                    self.content.insert(b, *c);
                    self.cursor += 1;
                }
            }
            Event::Key(Key::Backspace) if self.cursor > 0 => {
                self.cursor -= 1;
                let b = self.byte_at(self.cursor);
                self.content.remove(b);
            }
            Event::Key(Key::Backspace) => {}
            Event::Key(Key::Del) if self.cursor < self.len() => {
                let b = self.byte_at(self.cursor);
                self.content.remove(b);
            }
            Event::Key(Key::Del) => {}
            Event::Key(Key::Left) if self.cursor > 0 => self.cursor -= 1,
            Event::Key(Key::Right) if self.cursor < self.len() => self.cursor += 1,
            Event::Key(Key::Home) => self.cursor = 0,
            Event::Key(Key::End) => self.cursor = self.len(),
            Event::CtrlChar('u') => self.set_content(""),
            e if e.is_enter() => {
                let Some(f) = self.on_submit.clone() else {
                    return EventResult::Ignored;
                };
                let text = self.content.clone();
                return EventResult::with_cb(Rc::new(move |s| f(s, &text)));
            }
            _ => return EventResult::Ignored,
        }
        EventResult::consumed()
    }

    fn take_focus(&mut self, _dir: Direction) -> bool {
        true
    }

    fn important_row(&self) -> Option<usize> {
        Some(0)
    }
}

// --------------------------------------------------------------------------------------------
// Checkbox, Button

/// `[X]` / `[ ]`, toggled with Space or Enter.
#[derive(Default)]
pub struct Checkbox {
    checked: bool,
    on_change: Option<OnToggle>,
}

impl Checkbox {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn checked(mut self) -> Self {
        self.checked = true;
        self
    }

    pub fn with_checked(mut self, checked: bool) -> Self {
        self.checked = checked;
        self
    }

    pub fn set_checked(&mut self, checked: bool) {
        self.checked = checked;
    }

    pub fn is_checked(&self) -> bool {
        self.checked
    }

    pub fn on_change<F>(mut self, f: F) -> Self
    where
        F: Fn(&mut Ui, bool) + 'static,
    {
        self.on_change = Some(Rc::new(f));
        self
    }
}

impl View for Checkbox {
    fn draw(&self, p: &Printer) {
        let style = if p.focused {
            p.theme.highlight()
        } else {
            p.theme.primary()
        };
        let mark = if self.checked { "[X]" } else { "[ ]" };
        p.with_style(style, |p| p.print((0, 0), mark));
    }

    fn required_size(&mut self, _c: Vec2) -> Vec2 {
        Vec2::new(3, 1)
    }

    fn on_event(&mut self, e: &Event) -> EventResult {
        if *e != Event::Char(' ') && !e.is_enter() && !is_press(e) {
            return EventResult::Ignored;
        }
        self.checked = !self.checked;
        match self.on_change.clone() {
            Some(f) => {
                let on = self.checked;
                EventResult::with_cb(Rc::new(move |s| f(s, on)))
            }
            None => EventResult::consumed(),
        }
    }

    fn take_focus(&mut self, _dir: Direction) -> bool {
        true
    }

    fn important_row(&self) -> Option<usize> {
        Some(0)
    }
}

/// `<Label>`, Enter runs the callback.
pub struct Button {
    label: String,
    cb: Callback,
}

impl Button {
    pub fn new<F>(label: impl Into<String>, f: F) -> Self
    where
        F: Fn(&mut Ui) + 'static,
    {
        Button {
            label: label.into(),
            cb: Rc::new(f),
        }
    }
}

fn draw_button(p: &Printer, x: usize, label: &str, focused: bool) {
    let style = if focused {
        p.theme.highlight()
    } else {
        p.theme.primary()
    };
    p.with_style(style, |p| p.print((x, 0), &format!("<{label}>")));
}

impl View for Button {
    fn draw(&self, p: &Printer) {
        draw_button(p, 0, &self.label, p.focused);
    }

    fn required_size(&mut self, _c: Vec2) -> Vec2 {
        Vec2::new(width(&self.label) + 2, 1)
    }

    fn on_event(&mut self, e: &Event) -> EventResult {
        if e.is_enter() || is_press(e) {
            EventResult::with_cb(Rc::clone(&self.cb))
        } else {
            EventResult::Ignored
        }
    }

    fn take_focus(&mut self, _dir: Direction) -> bool {
        true
    }

    fn important_row(&self) -> Option<usize> {
        Some(0)
    }
}

// --------------------------------------------------------------------------------------------
// SelectView

/// A list to pick from (Enter submits), or with `popup()` one line that opens the list.
pub struct SelectView<T> {
    items: Vec<(String, Rc<T>)>,
    selected: Rc<Cell<usize>>,
    on_submit: Option<OnPick<T>>,
    on_select: Option<OnPick<T>>,
    popup: bool,
}

impl<T: 'static> Default for SelectView<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: 'static> SelectView<T> {
    pub fn new() -> Self {
        SelectView {
            items: Vec::new(),
            selected: Rc::new(Cell::new(0)),
            on_submit: None,
            on_select: None,
            popup: false,
        }
    }

    pub fn popup(mut self) -> Self {
        self.popup = true;
        self
    }

    pub fn add_item(&mut self, label: impl Into<String>, value: T) {
        self.items.push((label.into(), Rc::new(value)));
    }

    pub fn on_submit<F>(mut self, f: F) -> Self
    where
        F: Fn(&mut Ui, &T) + 'static,
    {
        self.on_submit = Some(Rc::new(f));
        self
    }

    /// Called when the user changes the selection (not by [`SelectView::set_selection`]).
    pub fn on_select<F>(mut self, f: F) -> Self
    where
        F: Fn(&mut Ui, &T) + 'static,
    {
        self.on_select = Some(Rc::new(f));
        self
    }

    pub fn selection(&self) -> Option<Rc<T>> {
        self.items.get(self.selected()).map(|(_, v)| Rc::clone(v))
    }

    pub fn set_selection(&mut self, i: usize) {
        if i < self.items.len() {
            self.selected.set(i);
        }
    }

    fn selected(&self) -> usize {
        self.selected.get().min(self.items.len().saturating_sub(1))
    }

    fn label_width(&self) -> usize {
        self.items.iter().map(|(l, _)| width(l)).max().unwrap_or(0)
    }

    fn select(&mut self, i: usize) -> EventResult {
        if i == self.selected() || i >= self.items.len() {
            return EventResult::Ignored;
        }
        self.selected.set(i);
        match self.on_select.clone() {
            Some(f) => {
                let item = Rc::clone(&self.items[i].1);
                EventResult::with_cb(Rc::new(move |s| f(s, &item)))
            }
            None => EventResult::consumed(),
        }
    }

    /// The submit callback for row `i`.
    fn submit(&self, i: usize) -> EventResult {
        let Some(f) = self.on_submit.clone() else {
            return EventResult::Ignored;
        };
        let item = Rc::clone(&self.items[i].1);
        EventResult::with_cb(Rc::new(move |s| f(s, &item)))
    }

    /// Callback that opens the list of a popup select; the choice goes into `selected`.
    fn open_popup(&self) -> Callback {
        let labels: Vec<String> = self.items.iter().map(|(l, _)| l.clone()).collect();
        let values: Vec<Rc<T>> = self.items.iter().map(|(_, v)| Rc::clone(v)).collect();
        let on_select = self.on_select.clone();
        let cell = Rc::clone(&self.selected);
        Rc::new(move |s: &mut Ui| {
            let mut list = SelectView::<usize>::new();
            for (i, l) in labels.iter().enumerate() {
                list.add_item(l.clone(), i);
            }
            list.set_selection(cell.get());
            let cell = Rc::clone(&cell);
            let values = values.clone();
            let on_select = on_select.clone();
            let list = list.on_submit(move |s, i| {
                let changed = cell.get() != *i;
                cell.set(*i);
                s.pop_layer();
                if let (true, Some(f)) = (changed, &on_select) {
                    f(s, &values[*i]);
                }
            });
            s.add_layer(
                OnEventView::new(Dialog::around(ScrollView::new(list)).title("")).on_event(
                    Key::Esc,
                    |s| {
                        s.pop_layer();
                    },
                ),
            );
        })
    }
}

impl<T: 'static> View for SelectView<T> {
    fn draw(&self, p: &Printer) {
        let w = p.size.x;
        if self.popup {
            let label = self
                .items
                .get(self.selected())
                .map(|(l, _)| l.as_str())
                .unwrap_or("");
            let style = if p.focused {
                p.theme.highlight()
            } else {
                p.theme.secondary()
            };
            let inner = w.saturating_sub(2);
            let text = format!(
                "<{:<inner$}>",
                crate::report::ellipsize(label, inner),
                inner = inner
            );
            p.with_style(style, |p| p.print((0, 0), &text));
            return;
        }
        let sel = self.selected();
        for (i, (label, _)) in self.items.iter().enumerate() {
            let line = format!("{label:<w$}");
            if i == sel {
                let style = if p.focused {
                    p.theme.highlight()
                } else {
                    p.theme.highlight_inactive()
                };
                p.with_style(style, |p| p.print((0, i), &line));
            } else {
                p.print((0, i), &line);
            }
        }
    }

    fn required_size(&mut self, _c: Vec2) -> Vec2 {
        if self.popup {
            Vec2::new(self.label_width() + 2, 1)
        } else {
            Vec2::new(self.label_width(), self.items.len())
        }
    }

    fn on_event(&mut self, e: &Event) -> EventResult {
        if self.items.is_empty() {
            return EventResult::Ignored;
        }
        let sel = self.selected();
        let last = self.items.len() - 1;
        if self.popup {
            return match e {
                e if e.is_enter() || is_press(e) => EventResult::with_cb(self.open_popup()),
                Event::Key(Key::Left) if sel > 0 => self.select(sel - 1),
                Event::Key(Key::Right) => self.select(sel + 1),
                _ => EventResult::Ignored,
            };
        }
        // A click selects a row; a click on the selected row submits it.
        if let Some((pos, MouseKind::Press)) = e.mouse() {
            if pos.y > last {
                return EventResult::Ignored;
            }
            if pos.y != sel {
                return self.select(pos.y);
            }
            return match self.submit(sel) {
                EventResult::Ignored => EventResult::consumed(),
                r => r,
            };
        }
        match e {
            Event::Key(Key::Up) if sel > 0 => self.select(sel - 1),
            Event::Key(Key::Down) => self.select(sel + 1),
            Event::Key(Key::PageUp) => self.select(sel.saturating_sub(10)),
            Event::Key(Key::PageDown) => self.select((sel + 10).min(last)),
            Event::Key(Key::Home) => self.select(0),
            Event::Key(Key::End) => self.select(last),
            e if e.is_enter() => self.submit(sel),
            _ => EventResult::Ignored,
        }
    }

    fn take_focus(&mut self, _dir: Direction) -> bool {
        !self.items.is_empty()
    }

    fn important_row(&self) -> Option<usize> {
        Some(if self.popup { 0 } else { self.selected() })
    }
}

// --------------------------------------------------------------------------------------------
// Dialog, Panel

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DialogFocus {
    Content,
    Button(usize),
}

/// A bordered box with a title, a content view and buttons at the bottom right.
pub struct Dialog {
    title: String,
    content: Box<dyn View>,
    content_focusable: bool,
    buttons: Vec<(String, Callback)>,
    focus: DialogFocus,
    /// Size at the last layout, for mouse clicks.
    size: Vec2,
}

impl Dialog {
    pub fn around<V: View>(content: V) -> Self {
        let mut content: Box<dyn View> = Box::new(content);
        let content_focusable = content.take_focus(Direction::Front);
        Dialog {
            title: String::new(),
            content,
            content_focusable,
            buttons: Vec::new(),
            focus: DialogFocus::Content,
            size: Vec2::default(),
        }
    }

    /// A dialog with only a text (no buttons).
    pub fn text(text: impl Into<String>) -> Self {
        Self::around(TextView::new(text))
    }

    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    pub fn button<F>(mut self, label: impl Into<String>, f: F) -> Self
    where
        F: Fn(&mut Ui) + 'static,
    {
        self.buttons.push((label.into(), Rc::new(f)));
        if !self.content_focusable && self.buttons.len() == 1 {
            self.focus = DialogFocus::Button(0);
        }
        self
    }

    /// A button that closes the dialog.
    pub fn dismiss_button(self, label: impl Into<String>) -> Self {
        self.button(label, |s| {
            s.pop_layer();
        })
    }

    fn button_rows(&self) -> usize {
        if self.buttons.is_empty() {
            0
        } else {
            2
        }
    }

    fn buttons_width(&self) -> usize {
        let n = self.buttons.len();
        self.buttons
            .iter()
            .map(|(l, _)| width(l) + 2)
            .sum::<usize>()
            + n.saturating_sub(1)
    }

    /// Start column of each button in a dialog `w` columns wide.
    fn button_xs(&self, w: usize) -> Vec<usize> {
        let mut x = w.saturating_sub(2 + self.buttons_width());
        self.buttons
            .iter()
            .map(|(label, _)| {
                let at = x;
                x += width(label) + 3;
                at
            })
            .collect()
    }

    /// A click on a button presses it; clicks and the wheel in the content go there.
    fn on_mouse(&mut self, e: &Event, pos: Vec2, kind: MouseKind) -> EventResult {
        let rows = self.button_rows();
        if rows > 0 && self.size.y >= 3 && pos.y == self.size.y - 2 {
            if kind != MouseKind::Press {
                return EventResult::Ignored;
            }
            let xs = self.button_xs(self.size.x);
            for (i, ((label, cb), x)) in self.buttons.iter().zip(xs).enumerate() {
                if (x..x + width(label) + 2).contains(&pos.x) {
                    self.focus = DialogFocus::Button(i);
                    return EventResult::with_cb(Rc::clone(cb));
                }
            }
            return EventResult::Ignored;
        }
        let inner = Vec2::new(
            self.size.x.saturating_sub(4),
            self.size.y.saturating_sub(2 + rows),
        );
        let Some(rel) = e.mouse_in(Vec2::new(2, 1), inner) else {
            return EventResult::Ignored;
        };
        let r = self.content.on_event(&rel);
        if r.is_consumed() && kind == MouseKind::Press && self.content_focusable {
            self.focus = DialogFocus::Content;
        }
        r
    }

    fn focus_content(&mut self, dir: Direction) -> bool {
        if self.content_focusable && self.content.take_focus(dir) {
            self.focus = DialogFocus::Content;
            true
        } else {
            false
        }
    }
}

impl View for Dialog {
    fn draw(&self, p: &Printer) {
        draw_border(p, &self.title);
        let rows = self.button_rows();
        let inner = Vec2::new(
            p.size.x.saturating_sub(4),
            p.size.y.saturating_sub(2 + rows),
        );
        let content_p = p
            .sub(Vec2::new(2, 1), inner)
            .focused(self.focus == DialogFocus::Content);
        self.content.draw(&content_p);
        if rows > 0 && p.size.y >= 3 {
            let y = p.size.y - 2;
            let row = p.sub(Vec2::new(0, y), Vec2::new(p.size.x, 1));
            for (i, ((label, _), x)) in self
                .buttons
                .iter()
                .zip(self.button_xs(p.size.x))
                .enumerate()
            {
                let focused = p.focused && self.focus == DialogFocus::Button(i);
                draw_button(&row, x, label, focused);
            }
        }
    }

    fn required_size(&mut self, c: Vec2) -> Vec2 {
        let rows = self.button_rows();
        let inner_c = Vec2::new(c.x.saturating_sub(4), c.y.saturating_sub(2 + rows));
        let cs = self.content.required_size(inner_c).min(inner_c);
        let w = cs.x.max(self.buttons_width()).max(width(&self.title) + 4);
        Vec2::new(w + 4, cs.y + 2 + rows)
    }

    fn layout(&mut self, size: Vec2) {
        self.size = size;
        let rows = self.button_rows();
        self.content.layout(Vec2::new(
            size.x.saturating_sub(4),
            size.y.saturating_sub(2 + rows),
        ));
    }

    fn on_event(&mut self, e: &Event) -> EventResult {
        if let Some((pos, kind)) = e.mouse() {
            return self.on_mouse(e, pos, kind);
        }
        let n = self.buttons.len();
        match self.focus {
            DialogFocus::Content => {
                let r = self.content.on_event(e);
                if r.is_consumed() || n == 0 {
                    return r;
                }
                match e {
                    Event::Key(Key::Tab | Key::Down) => self.focus = DialogFocus::Button(0),
                    Event::Shift(Key::Tab) => self.focus = DialogFocus::Button(n - 1),
                    _ => return EventResult::Ignored,
                }
                EventResult::consumed()
            }
            DialogFocus::Button(i) => {
                match e {
                    e if e.is_enter() => {
                        return EventResult::with_cb(Rc::clone(&self.buttons[i].1));
                    }
                    Event::Key(Key::Left) if i > 0 => self.focus = DialogFocus::Button(i - 1),
                    Event::Key(Key::Right) if i + 1 < n => self.focus = DialogFocus::Button(i + 1),
                    Event::Key(Key::Left | Key::Right) => {}
                    Event::Key(Key::Tab) => {
                        if i + 1 < n {
                            self.focus = DialogFocus::Button(i + 1);
                        } else if !self.focus_content(Direction::Front) {
                            self.focus = DialogFocus::Button(0);
                        }
                    }
                    Event::Shift(Key::Tab) => {
                        if i > 0 {
                            self.focus = DialogFocus::Button(i - 1);
                        } else if !self.focus_content(Direction::Back) {
                            self.focus = DialogFocus::Button(n - 1);
                        }
                    }
                    Event::Key(Key::Up) if self.content_focusable => {
                        if !self.focus_content(Direction::Back) {
                            return EventResult::Ignored;
                        }
                    }
                    // A text without fields scrolls while a button has the focus.
                    e if is_scroll_key(e) && !self.content_focusable => {
                        return self.content.on_event(e);
                    }
                    _ => return EventResult::Ignored,
                }
                EventResult::consumed()
            }
        }
    }

    fn take_focus(&mut self, dir: Direction) -> bool {
        if (dir == Direction::Front || self.buttons.is_empty()) && self.focus_content(dir) {
            return true;
        }
        if self.buttons.is_empty() {
            return false;
        }
        self.focus = DialogFocus::Button(match dir {
            Direction::Front => 0,
            Direction::Back => self.buttons.len() - 1,
        });
        true
    }

    fn call_on_any(&mut self, name: &str, f: &mut dyn FnMut(&mut dyn Any)) {
        self.content.call_on_any(name, f)
    }
}

/// A border with a title around a view (the Status screen).
pub struct Panel<V> {
    inner: V,
    title: String,
    size: Vec2,
}

impl<V: View> Panel<V> {
    pub fn new(inner: V) -> Self {
        Panel {
            inner,
            title: String::new(),
            size: Vec2::default(),
        }
    }

    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }
}

impl<V: View> View for Panel<V> {
    fn draw(&self, p: &Printer) {
        draw_border(p, &self.title);
        let inner = p.sub(Vec2::new(1, 1), p.size.saturating_sub(Vec2::new(2, 2)));
        self.inner.draw(&inner);
    }

    fn required_size(&mut self, c: Vec2) -> Vec2 {
        let r = self.inner.required_size(c.saturating_sub(Vec2::new(2, 2)));
        Vec2::new(r.x + 2, r.y + 2)
    }

    fn layout(&mut self, size: Vec2) {
        self.size = size;
        self.inner.layout(size.saturating_sub(Vec2::new(2, 2)))
    }

    fn on_event(&mut self, e: &Event) -> EventResult {
        if e.mouse().is_some() {
            let inner = self.size.saturating_sub(Vec2::new(2, 2));
            return match e.mouse_in(Vec2::new(1, 1), inner) {
                Some(rel) => self.inner.on_event(&rel),
                None => EventResult::Ignored,
            };
        }
        self.inner.on_event(e)
    }

    fn take_focus(&mut self, dir: Direction) -> bool {
        self.inner.take_focus(dir)
    }

    fn call_on_any(&mut self, name: &str, f: &mut dyn FnMut(&mut dyn Any)) {
        self.inner.call_on_any(name, f)
    }
}

// --------------------------------------------------------------------------------------------
// OnEventView, NamedView

/// Key bindings around a view; they apply when the view does not use the key.
pub struct OnEventView<V> {
    inner: V,
    callbacks: Vec<(Event, Callback)>,
}

impl<V: View> OnEventView<V> {
    pub fn new(inner: V) -> Self {
        OnEventView {
            inner,
            callbacks: Vec::new(),
        }
    }

    pub fn on_event<F>(mut self, event: impl Into<Event>, f: F) -> Self
    where
        F: Fn(&mut Ui) + 'static,
    {
        self.callbacks.push((event.into(), Rc::new(f)));
        self
    }
}

impl<V: View> View for OnEventView<V> {
    fn draw(&self, p: &Printer) {
        self.inner.draw(p)
    }

    fn required_size(&mut self, c: Vec2) -> Vec2 {
        self.inner.required_size(c)
    }

    fn layout(&mut self, size: Vec2) {
        self.inner.layout(size)
    }

    fn on_event(&mut self, e: &Event) -> EventResult {
        let r = self.inner.on_event(e);
        if r.is_consumed() {
            return r;
        }
        match self.callbacks.iter().find(|(ev, _)| ev == e) {
            Some((_, cb)) => EventResult::with_cb(Rc::clone(cb)),
            None => EventResult::Ignored,
        }
    }

    fn take_focus(&mut self, dir: Direction) -> bool {
        self.inner.take_focus(dir)
    }

    fn call_on_any(&mut self, name: &str, f: &mut dyn FnMut(&mut dyn Any)) {
        self.inner.call_on_any(name, f)
    }

    fn important_row(&self) -> Option<usize> {
        self.inner.important_row()
    }
}

/// A view with a name (see [`super::Ui::call_on_name`]).
pub struct NamedView<V> {
    name: String,
    inner: V,
}

impl<V: View> NamedView<V> {
    pub fn new(name: impl Into<String>, inner: V) -> Self {
        NamedView {
            name: name.into(),
            inner,
        }
    }
}

impl<V: View> View for NamedView<V> {
    fn draw(&self, p: &Printer) {
        self.inner.draw(p)
    }

    fn required_size(&mut self, c: Vec2) -> Vec2 {
        self.inner.required_size(c)
    }

    fn layout(&mut self, size: Vec2) {
        self.inner.layout(size)
    }

    fn on_event(&mut self, e: &Event) -> EventResult {
        self.inner.on_event(e)
    }

    fn take_focus(&mut self, dir: Direction) -> bool {
        self.inner.take_focus(dir)
    }

    fn call_on_any(&mut self, name: &str, f: &mut dyn FnMut(&mut dyn Any)) {
        if self.name == name {
            f(&mut self.inner);
        } else {
            self.inner.call_on_any(name, f);
        }
    }

    fn important_row(&self) -> Option<usize> {
        self.inner.important_row()
    }
}
