use crate::job_view;
use crossterm::event::KeyCode;
use ratatui::{layout::Rect, text::Span};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Key(KeyCode),
    Row(String),
    Fold(String),
    JobSort(job_view::Sort),
    Sort(usize),
    Pane(usize),
    History(usize),
    Menu(usize),
    Choice(usize),
    Close,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Main { tab: usize, searching: bool },
    Popup(u64),
}

#[derive(Default)]
pub struct Hits {
    pub scope: Option<Scope>,
    pub scroll: Rect,
    pub items: Vec<(Rect, Action)>,
}
impl Hits {
    pub fn clear(&mut self) {
        *self = Self::default();
    }
    pub fn reset(&mut self, scope: Scope) {
        self.clear();
        self.scope = Some(scope);
    }
    pub fn add(&mut self, area: Rect, action: Action) {
        if !area.is_empty() {
            self.items.push((area, action));
        }
    }
    pub fn at(&self, x: u16, y: u16) -> Option<Action> {
        self.items
            .iter()
            .rev()
            .find(|(area, _)| area.contains((x, y).into()))
            .map(|(_, action)| action.clone())
    }
    pub fn spans(&mut self, area: Rect, spans: &[Span<'_>], actions: &[Option<Action>]) {
        let mut x = area.x;
        for (span, action) in spans.iter().zip(actions) {
            let width = span.width().min(u16::MAX as usize) as u16;
            if let Some(action) = action {
                self.add(
                    Rect::new(
                        x,
                        area.y,
                        width.min(area.right().saturating_sub(x)),
                        area.height.min(1),
                    ),
                    action.clone(),
                );
            }
            x = x.saturating_add(width).min(area.right());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_labels_and_clipped_actions_match_terminal_cells() {
        let mut hits = Hits::default();
        hits.spans(
            Rect::new(10, 3, 8, 1),
            &[Span::raw("사용자"), Span::raw("XYZ")],
            &[
                Some(Action::Key(KeyCode::Char('u'))),
                Some(Action::Key(KeyCode::Char('x'))),
            ],
        );
        assert_eq!(hits.at(15, 3), Some(Action::Key(KeyCode::Char('u'))));
        assert_eq!(hits.at(16, 3), Some(Action::Key(KeyCode::Char('x'))));
        assert_eq!(hits.at(17, 3), Some(Action::Key(KeyCode::Char('x'))));
        assert_eq!(hits.at(18, 3), None);
        assert_eq!(hits.at(10, 4), None);
        hits.clear();
        hits.spans(
            Rect::new(0, 0, 8, 0),
            &[Span::raw("hidden")],
            &[Some(Action::Key(KeyCode::Enter))],
        );
        assert!(hits.items.is_empty());
    }
}
