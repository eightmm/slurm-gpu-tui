use crossterm::event::KeyCode;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Style,
    text::{Line, Span, Text},
    widgets::{Paragraph, Widget, Wrap},
};
use regex::RegexBuilder;

#[derive(Default)]
pub struct Find {
    pub query: String,
    pub editing: bool,
    pub rows: Vec<u16>,
    pub selected: usize,
    jump: bool,
    cached_body: String,
    cached_query: String,
    cached_width: u16,
}
impl Find {
    pub fn input(&mut self, key: KeyCode) -> bool {
        if !self.editing {
            return false;
        }
        match key {
            KeyCode::Enter => {
                self.editing = false;
                return true;
            }
            KeyCode::Esc => {
                self.editing = false;
                self.query.clear();
            }
            KeyCode::Backspace => {
                self.query.pop();
            }
            KeyCode::Char(c) if !c.is_control() => {
                if self.query.len() + c.len_utf8() <= 256 {
                    self.query.push(c);
                }
            }
            _ => return false,
        }
        self.selected = 0;
        self.jump = true;
        true
    }
    pub fn next(&mut self, backwards: bool) {
        if !self.rows.is_empty() {
            self.selected = if backwards {
                (self.selected + self.rows.len() - 1) % self.rows.len()
            } else {
                (self.selected + 1) % self.rows.len()
            };
            self.jump = true;
        }
    }
    pub fn pane_changed(&mut self) {
        self.editing = false;
        self.selected = 0;
        self.cached_width = 0;
        self.jump = false;
    }
    pub fn take_jump(&mut self) -> Option<u16> {
        let jump = std::mem::take(&mut self.jump);
        jump.then(|| self.rows.get(self.selected).copied())
            .flatten()
    }
    pub fn text(
        &mut self,
        raw: &str,
        width: u16,
        normal: Style,
        error: Style,
        matched: Style,
    ) -> Text<'static> {
        let pattern = (!self.query.is_empty())
            .then(|| {
                RegexBuilder::new(&regex::escape(&self.query))
                    .case_insensitive(true)
                    .build()
                    .ok()
            })
            .flatten();
        let lines = raw
            .lines()
            .map(|line| {
                let style = if ["Error", "ERROR", "Traceback", "Exception", "OOM", "FAILED"]
                    .iter()
                    .any(|word| line.contains(word))
                {
                    error
                } else {
                    normal
                };
                let mut spans = Vec::new();
                let mut start = 0;
                if let Some(pattern) = &pattern {
                    for found in pattern.find_iter(line) {
                        spans.push(Span::styled(line[start..found.start()].to_string(), style));
                        spans.push(Span::styled(found.as_str().to_string(), matched));
                        start = found.end();
                    }
                }
                spans.push(Span::styled(line[start..].to_string(), style));
                Line::from(spans)
            })
            .collect::<Vec<_>>();
        let text = Text::from(lines);
        if pattern.is_none() {
            self.rows.clear();
            self.cached_body.clear();
            self.cached_query.clear();
            self.cached_width = 0;
            return text;
        }
        if self.cached_body != raw || self.cached_query != self.query || self.cached_width != width
        {
            if self.cached_width != 0 && self.cached_width != width {
                self.jump = true;
            }
            self.rows.clear();
            if pattern.is_some() && width > 0 {
                let mut base = 0usize;
                for line in &text.lines {
                    let paragraph = Paragraph::new(line.clone()).wrap(Wrap { trim: false });
                    let count = paragraph.line_count(width);
                    if line.spans.iter().any(|span| span.style.bg == matched.bg) {
                        // Use the same renderer as the popup, in bounded tiles, so
                        // word wrapping and wide characters share exact search coordinates.
                        let tile = (32768 / width as usize).clamp(1, 128);
                        for offset in (0..count.min(u16::MAX as usize + 1)).step_by(tile) {
                            let height = tile.min(count - offset) as u16;
                            let area = Rect::new(0, 0, width, height);
                            let mut buffer = Buffer::empty(area);
                            paragraph
                                .clone()
                                .scroll((offset as u16, 0))
                                .render(area, &mut buffer);
                            for (row, cells) in buffer.content.chunks(width as usize).enumerate() {
                                if cells.iter().any(|cell| Some(cell.bg) == matched.bg)
                                    && let Ok(row) = u16::try_from(base + offset + row)
                                {
                                    self.rows.push(row);
                                }
                            }
                        }
                    }
                    base = base.saturating_add(count);
                }
            }
            self.rows.dedup();
            self.selected = self.selected.min(self.rows.len().saturating_sub(1));
            self.cached_body = raw.into();
            self.cached_query = self.query.clone();
            self.cached_width = width;
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;
    #[test]
    fn literal_unicode_search_tracks_native_wrapping_and_next_previous() {
        let mut find = Find {
            editing: true,
            ..Default::default()
        };
        for c in "오류[0]".chars() {
            assert!(find.input(KeyCode::Char(c)));
        }
        let normal = Style::default().fg(Color::White);
        let matched = Style::default().bg(Color::Yellow);
        let raw = "first\n\nlong/path/오류[0]/tail tail\n오류[0]";
        let text = find.text(raw, 8, normal, normal, matched);
        assert!(!find.rows.is_empty());
        let row = find.take_jump().unwrap();
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 1));
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .scroll((row, 0))
            .render(buffer.area, &mut buffer);
        assert!(buffer.content.iter().any(|cell| cell.bg == Color::Yellow));
        find.next(false);
        assert!(find.take_jump().unwrap() >= row);
        find.next(true);
        assert_eq!(find.take_jump(), Some(row));
        find.input(KeyCode::Esc);
        find.text(raw, 8, normal, normal, matched);
        assert!(find.rows.is_empty());
    }
}
