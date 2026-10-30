//! Drawing into the frame buffer: a view's area, clipped to what is visible (scrolling).

use std::cell::{Cell, RefCell};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use unicode_width::UnicodeWidthStr;

use super::{Theme, Vec2};

/// Draws a view: positions are relative to the view, everything outside `clip` is dropped.
#[derive(Clone)]
pub struct Printer<'a> {
    buf: &'a RefCell<Buffer>,
    pub theme: &'a Theme,
    cursor: &'a Cell<Option<(u16, u16)>>,
    /// Absolute screen position of the view's (0, 0); above `clip` when scrolled down.
    origin: (i32, i32),
    /// Visible part of the screen, absolute.
    clip: Rect,
    /// Size of the view.
    pub size: Vec2,
    /// Whether the view has the keyboard focus.
    pub focused: bool,
    style: Style,
}

impl<'a> Printer<'a> {
    pub fn new(
        buf: &'a RefCell<Buffer>,
        theme: &'a Theme,
        cursor: &'a Cell<Option<(u16, u16)>>,
        area: Rect,
    ) -> Self {
        Printer {
            buf,
            theme,
            cursor,
            origin: (area.x.into(), area.y.into()),
            clip: area,
            size: Vec2::new(area.width.into(), area.height.into()),
            focused: true,
            style: theme.primary(),
        }
    }

    /// A printer for the part of this view at `offset` with `size`.
    pub fn sub(&self, offset: Vec2, size: Vec2) -> Printer<'a> {
        let size = size.min(self.size.saturating_sub(offset));
        let origin = (
            self.origin.0 + offset.x as i32,
            self.origin.1 + offset.y as i32,
        );
        Printer {
            origin,
            clip: intersect(self.clip, origin, size),
            size,
            ..self.clone()
        }
    }

    /// A printer for content of `size` scrolled down by `dy` rows inside this view.
    pub fn scrolled(&self, dy: usize, size: Vec2) -> Printer<'a> {
        Printer {
            origin: (self.origin.0, self.origin.1 - dy as i32),
            clip: intersect(self.clip, self.origin, self.size),
            size,
            ..self.clone()
        }
    }

    pub fn focused(&self, focused: bool) -> Printer<'a> {
        Printer {
            focused: self.focused && focused,
            ..self.clone()
        }
    }

    /// Run `f` with another style.
    pub fn with_style<F: FnOnce(&Printer)>(&self, style: Style, f: F) {
        let p = Printer {
            style,
            ..self.clone()
        };
        f(&p);
    }

    /// Print `text` (one line) at `pos`, cut at the right edge of the view.
    pub fn print(&self, pos: impl Into<Vec2>, text: &str) {
        let pos = pos.into();
        if pos.x >= self.size.x || pos.y >= self.size.y {
            return;
        }
        let ax = self.origin.0 + pos.x as i32;
        let ay = self.origin.1 + pos.y as i32;
        let (cx, cy) = (i32::from(self.clip.x), i32::from(self.clip.y));
        if ay < cy || ay >= cy + i32::from(self.clip.height) || ax < cx {
            return;
        }
        let room = (cx + i32::from(self.clip.width) - ax).max(0) as usize;
        let max = (self.size.x - pos.x).min(room);
        if max == 0 {
            return;
        }
        self.buf
            .borrow_mut()
            .set_stringn(ax as u16, ay as u16, text, max, self.style);
    }

    /// Print `s` `len` times starting at `pos`.
    pub fn print_hline(&self, pos: impl Into<Vec2>, len: usize, s: &str) {
        if len > 0 {
            self.print(pos, &s.repeat(len));
        }
    }

    /// Fill the whole view with spaces in the current style.
    pub fn fill(&self) {
        for y in 0..self.size.y {
            self.print_hline((0, y), self.size.x, " ");
        }
    }

    /// Put the terminal cursor at `pos` (input fields) when this view has the focus.
    pub fn set_cursor(&self, pos: impl Into<Vec2>) {
        let pos = pos.into();
        if !self.focused || pos.x >= self.size.x || pos.y >= self.size.y {
            return;
        }
        let ax = self.origin.0 + pos.x as i32;
        let ay = self.origin.1 + pos.y as i32;
        let c = self.clip;
        if ax >= c.x.into()
            && ay >= c.y.into()
            && ax < i32::from(c.x) + i32::from(c.width)
            && ay < i32::from(c.y) + i32::from(c.height)
        {
            self.cursor.set(Some((ax as u16, ay as u16)));
        }
    }
}

/// `clip` intersected with the rectangle at `origin` with `size` (may start above the screen).
fn intersect(clip: Rect, origin: (i32, i32), size: Vec2) -> Rect {
    let x0 = origin.0.max(clip.x.into());
    let y0 = origin.1.max(clip.y.into());
    let x1 = (origin.0 + size.x as i32).min(i32::from(clip.x) + i32::from(clip.width));
    let y1 = (origin.1 + size.y as i32).min(i32::from(clip.y) + i32::from(clip.height));
    if x1 <= x0 || y1 <= y0 {
        return Rect::new(x0.max(0) as u16, y0.max(0) as u16, 0, 0);
    }
    Rect::new(x0 as u16, y0 as u16, (x1 - x0) as u16, (y1 - y0) as u16)
}

/// Display width of `s` in cells.
pub fn width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Wrap `text` to `width` cells: explicit line breaks and indentation are kept, long lines
/// break at the last space (or hard when a word is longer than the line).
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let text = text.strip_suffix('\n').unwrap_or(text);
    let mut out = Vec::new();
    for line in text.split('\n') {
        let line = line.trim_end_matches('\r');
        if self::width(line) <= width {
            out.push(line.to_string());
            continue;
        }
        let mut rest: &str = line;
        while self::width(rest) > width {
            // Longest prefix that fits, in chars.
            let mut fit = 0;
            let mut w = 0;
            for (i, c) in rest.char_indices() {
                let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
                if w + cw > width {
                    break;
                }
                w += cw;
                fit = i + c.len_utf8();
            }
            let cut = match rest[..fit].rfind(' ') {
                // The line ends exactly at a word end.
                _ if rest[fit..].starts_with(' ') && fit > 0 => fit,
                Some(sp) if sp > 0 && !rest[..sp].trim().is_empty() => sp,
                _ => fit.max(rest.chars().next().map_or(1, char::len_utf8)),
            };
            out.push(rest[..cut].trim_end().to_string());
            rest = rest[cut..].trim_start_matches(' ');
        }
        if !rest.is_empty() {
            out.push(rest.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_keeps_lines_and_breaks_at_spaces() {
        assert_eq!(wrap("ab\n  cd", 10), vec!["ab", "  cd"]);
        assert_eq!(wrap("one two three", 7), vec!["one two", "three"]);
        assert_eq!(wrap("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        assert_eq!(wrap("", 4), vec![""]);
        assert_eq!(wrap("x\n", 4), vec!["x"]);
    }

    #[test]
    fn intersect_clips_scrolled_content() {
        let clip = Rect::new(2, 2, 10, 5);
        assert_eq!(
            intersect(clip, (2, -3), Vec2::new(10, 20)),
            Rect::new(2, 2, 10, 5)
        );
        assert_eq!(
            intersect(clip, (4, 3), Vec2::new(3, 2)),
            Rect::new(4, 3, 3, 2)
        );
        assert_eq!(intersect(clip, (20, 3), Vec2::new(3, 2)).width, 0);
    }
}
