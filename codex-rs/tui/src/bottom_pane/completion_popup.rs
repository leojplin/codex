use std::ops::Range;

use ratatui::buffer::Buffer;
use ratatui::layout::Margin;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Block;
use ratatui::widgets::Borders;
use ratatui::widgets::Clear;
use ratatui::widgets::Widget;
use ratatui::widgets::WidgetRef;
use unicode_width::UnicodeWidthChar;
use unicode_width::UnicodeWidthStr;

use crate::style::accent_style;
use crate::style::user_message_style;

use super::completion_index::CompletionMatch;
use super::completion_index::MAX_COMPLETION_RESULTS;
use super::scroll_state::ScrollState;

#[derive(Debug)]
pub(crate) struct CompletionPopup {
    query: String,
    token_range: Range<usize>,
    matches: Vec<CompletionMatch>,
    state: ScrollState,
}

impl CompletionPopup {
    pub(crate) fn new(
        query: String,
        token_range: Range<usize>,
        matches: Vec<CompletionMatch>,
    ) -> Self {
        let mut state = ScrollState::new();
        state.clamp_selection(matches.len());
        Self {
            query,
            token_range,
            matches,
            state,
        }
    }

    pub(crate) fn set_results(
        &mut self,
        query: String,
        token_range: Range<usize>,
        matches: Vec<CompletionMatch>,
    ) {
        let query_changed = self.query != query;
        self.query = query;
        self.token_range = token_range;
        self.matches = matches;
        if query_changed {
            self.state.reset();
        }
        self.state.clamp_selection(self.matches.len());
        self.state
            .ensure_visible(self.matches.len(), MAX_COMPLETION_RESULTS);
    }

    pub(crate) fn move_next(&mut self) {
        self.state.move_down_wrap(self.matches.len());
        self.state
            .ensure_visible(self.matches.len(), MAX_COMPLETION_RESULTS);
    }

    pub(crate) fn move_previous(&mut self) {
        self.state.move_up_wrap(self.matches.len());
        self.state
            .ensure_visible(self.matches.len(), MAX_COMPLETION_RESULTS);
    }

    pub(crate) fn selected(&self) -> Option<(Range<usize>, String)> {
        self.state
            .selected_idx
            .and_then(|idx| self.matches.get(idx))
            .map(|completion_match| (self.token_range.clone(), completion_match.text.clone()))
    }

    pub(crate) fn token_query(&self) -> &str {
        &self.query
    }

    pub(crate) fn calculate_required_height(&self) -> u16 {
        let rows = self.matches.len().clamp(1, MAX_COMPLETION_RESULTS) as u16;
        rows.saturating_add(2)
    }

    pub(crate) fn preferred_width(&self) -> u16 {
        let content_width = self
            .matches
            .iter()
            .map(|completion_match| {
                completion_match.text.width() + completion_match.kind.label().len() + 4
            })
            .max()
            .unwrap_or(10);
        content_width.saturating_add(2).min(u16::MAX as usize) as u16
    }

    fn render_rows(&self, area: Rect, buf: &mut Buffer) {
        if self.matches.is_empty() {
            if area.height > 0 {
                Line::from("no matches".dim().italic()).render(area, buf);
            }
            return;
        }

        let visible_items = MAX_COMPLETION_RESULTS
            .min(self.matches.len())
            .min(area.height.max(1) as usize);
        let mut start_idx = self
            .state
            .scroll_top
            .min(self.matches.len().saturating_sub(1));
        if let Some(selected_idx) = self.state.selected_idx {
            if selected_idx < start_idx {
                start_idx = selected_idx;
            } else if visible_items > 0 {
                let bottom = start_idx + visible_items - 1;
                if selected_idx > bottom {
                    start_idx = selected_idx + 1 - visible_items;
                }
            }
        }

        for (row_offset, (idx, completion_match)) in self
            .matches
            .iter()
            .enumerate()
            .skip(start_idx)
            .take(visible_items)
            .enumerate()
        {
            let y = area.y.saturating_add(row_offset as u16);
            if y >= area.bottom() {
                break;
            }
            self.render_row(area.x, y, area.width, idx, completion_match, buf);
        }
    }

    fn render_row(
        &self,
        x: u16,
        y: u16,
        width: u16,
        idx: usize,
        completion_match: &CompletionMatch,
        buf: &mut Buffer,
    ) {
        if width == 0 {
            return;
        }

        let selected = Some(idx) == self.state.selected_idx;
        let tag = completion_match.kind.label();
        let tag_width = tag.width() as u16;
        let name_width = width.saturating_sub(tag_width.saturating_add(2));
        let mut name_line = completion_name_line(completion_match, name_width);
        let tag_x = x.saturating_add(width.saturating_sub(tag_width));
        let mut tag_span = Span::from(tag.to_string()).dim();

        if selected {
            name_line.spans.iter_mut().for_each(|span| {
                span.style = accent_style();
            });
            tag_span.style = accent_style();
        }

        buf.set_line(x, y, &name_line, name_width);
        if tag_width <= width {
            buf.set_span(tag_x, y, &tag_span, tag_width);
        }
    }
}

fn completion_name_line(completion_match: &CompletionMatch, width: u16) -> Line<'static> {
    if width == 0 {
        return Line::from("");
    }

    let mut spans = Vec::new();
    let mut used_width = 0usize;
    let mut truncated = false;
    let mut match_indices = completion_match.match_indices.iter().peekable();
    let limit = width as usize;

    for (char_idx, ch) in completion_match.text.chars().enumerate() {
        let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        let next_width = used_width.saturating_add(char_width);
        if next_width > limit {
            truncated = true;
            break;
        }
        used_width = next_width;

        if match_indices.peek().is_some_and(|idx| **idx == char_idx) {
            match_indices.next();
            spans.push(ch.to_string().bold());
        } else {
            spans.push(ch.to_string().into());
        }
    }

    if truncated {
        spans.push("…".into());
    }

    Line::from(spans)
}

impl WidgetRef for &CompletionPopup {
    fn render_ref(&self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() {
            return;
        }

        Clear.render(area, buf);
        let block = Block::default()
            .borders(Borders::ALL)
            .style(user_message_style());
        block.render(area, buf);
        let content_area = area.inner(Margin::new(1, 1));
        self.render_rows(content_area, buf);
    }
}
