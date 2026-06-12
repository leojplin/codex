use std::collections::VecDeque;

use color_eyre::eyre::Result;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;

use crate::terminal_hyperlinks::HyperlinkLine;
use crate::tui;

use super::App;

impl App {
    pub(super) fn invalidate_completion_overlay_viewport_rows(&self, tui: &mut tui::Tui) {
        let Some(rect) = self.completion_overlay_rect else {
            return;
        };

        let viewport = tui.terminal.viewport_area;
        if rect.y < viewport.bottom() && rect.bottom() > viewport.y {
            tui.terminal.invalidate_region(rect);
        }
    }

    pub(super) fn draw_completion_terminal_overlay(
        &mut self,
        tui: &mut tui::Tui,
        cursor_position: Option<(u16, u16)>,
    ) -> Result<()> {
        let terminal_size = tui.terminal.size()?;
        let overlay = cursor_position.and_then(|(cursor_x, cursor_y)| {
            self.chat_widget.completion_popup_overlay_buffer(
                cursor_x,
                cursor_y,
                terminal_size.width,
                terminal_size.height,
            )
        });
        let overlay_rect = overlay.as_ref().map(|buffer| *buffer.area());

        if self.completion_overlay_rect != overlay_rect
            && let Some(previous_rect) = self.completion_overlay_rect
        {
            self.restore_completion_overlay_rows(tui, previous_rect)?;
        }

        if let Some(buffer) = overlay.as_ref() {
            tui.draw_overlay_buffer(buffer)?;
        }
        self.completion_overlay_rect = overlay_rect;
        Ok(())
    }

    fn restore_completion_overlay_rows(&self, tui: &mut tui::Tui, rect: Rect) -> Result<()> {
        let viewport_top = tui.terminal.viewport_area.y;
        let start_y = rect.y.min(viewport_top);
        let end_y = rect.bottom().min(viewport_top);
        if start_y >= end_y {
            return Ok(());
        }

        let terminal_width = tui.terminal.size()?.width;
        if terminal_width == 0 {
            return Ok(());
        }

        let visible_rows = usize::from(viewport_top);
        let history_rows =
            self.visible_history_rows_for_completion_overlay(terminal_width, visible_rows);
        let mut restore = Buffer::empty(Rect::new(
            0,
            start_y,
            terminal_width,
            end_y.saturating_sub(start_y),
        ));
        for y in start_y..end_y {
            let line = history_rows
                .get(usize::from(y))
                .cloned()
                .unwrap_or_else(Line::default);
            restore.set_line(0, y, &line, terminal_width);
        }
        tui.draw_overlay_buffer(&restore)?;
        Ok(())
    }

    fn visible_history_rows_for_completion_overlay(
        &self,
        terminal_width: u16,
        visible_rows: usize,
    ) -> Vec<Line<'static>> {
        if visible_rows == 0 {
            return Vec::new();
        }

        let width = self.chat_widget.history_wrap_width(terminal_width);
        let rendered = self.transcript_tail_lines_for_completion_overlay(width, visible_rows);
        let tail_start = rendered.len().saturating_sub(visible_rows);
        let tail = &rendered[tail_start..];
        let mut rows = Vec::with_capacity(visible_rows);
        rows.resize_with(visible_rows.saturating_sub(tail.len()), Line::default);
        rows.extend(tail.iter().map(|line| line.line.clone()));
        rows
    }

    fn transcript_tail_lines_for_completion_overlay(
        &self,
        width: u16,
        row_cap: usize,
    ) -> Vec<HyperlinkLine> {
        if row_cap == 0 {
            return Vec::new();
        }

        let mut cell_displays = VecDeque::new();
        let mut rendered_rows = 0usize;
        let mut start = self.transcript_cells.len();
        while start > 0 {
            start -= 1;
            let cell = &self.transcript_cells[start];
            let lines = cell
                .display_hyperlink_lines_for_mode(width, self.chat_widget.history_render_mode());
            rendered_rows += lines.len();
            cell_displays.push_front((lines, cell.is_stream_continuation()));
            if rendered_rows > row_cap {
                break;
            }
        }

        while start > 0
            && cell_displays
                .front()
                .is_some_and(|(_, is_stream_continuation)| *is_stream_continuation)
        {
            start -= 1;
            let cell = &self.transcript_cells[start];
            cell_displays.push_front((
                cell.display_hyperlink_lines_for_mode(
                    width,
                    self.chat_widget.history_render_mode(),
                ),
                cell.is_stream_continuation(),
            ));
        }

        let mut has_emitted_history_lines = false;
        let mut lines = Vec::new();
        for (display, is_stream_continuation) in cell_displays {
            if !display.is_empty() && !is_stream_continuation {
                if has_emitted_history_lines {
                    lines.push(HyperlinkLine::new(Line::from("")));
                } else {
                    has_emitted_history_lines = true;
                }
            }
            lines.extend(display);
        }
        if lines.len() > row_cap {
            lines.split_off(lines.len() - row_cap)
        } else {
            lines
        }
    }
}
