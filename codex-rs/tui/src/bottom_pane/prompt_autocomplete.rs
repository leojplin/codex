use crate::history_cell::HistoryCell;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::WidgetRef;

use super::ChatComposer;
use super::InputResult;
use super::completion_index::SessionCompletionIndex;
use super::completion_popup::CompletionPopup;

pub(crate) struct PromptAutocomplete {
    index: SessionCompletionIndex,
    popup: Option<CompletionPopup>,
    dismissed_token: Option<String>,
    enabled: bool,
    dictionary_enabled: bool,
}

impl Default for PromptAutocomplete {
    fn default() -> Self {
        Self {
            index: SessionCompletionIndex::default(),
            popup: None,
            dismissed_token: None,
            enabled: true,
            dictionary_enabled: true,
        }
    }
}

impl PromptAutocomplete {
    pub(crate) fn set_config(
        &mut self,
        enabled: bool,
        dictionary_enabled: bool,
        composer: &ChatComposer,
        blocked: bool,
    ) -> bool {
        self.enabled = enabled;
        self.dictionary_enabled = dictionary_enabled;
        if !enabled {
            self.dismissed_token = None;
            self.clear_popup()
        } else {
            self.sync(composer, blocked)
        }
    }

    pub(crate) fn popup_active(&self) -> bool {
        self.popup.is_some()
    }

    pub(crate) fn clear_popup(&mut self) -> bool {
        self.popup.take().is_some()
    }

    pub(crate) fn sync(&mut self, composer: &ChatComposer, blocked: bool) -> bool {
        if !self.enabled {
            self.dismissed_token = None;
            return self.clear_popup();
        }

        if blocked {
            return self.clear_popup();
        }

        let Some(context) = composer.completion_context() else {
            self.dismissed_token = None;
            return self.clear_popup();
        };

        if self.dismissed_token.as_ref() == Some(&context.query) {
            return self.clear_popup();
        }

        let matches = self
            .index
            .search_with_dictionary(&context.query, self.dictionary_enabled);
        if matches.is_empty() {
            return self.clear_popup();
        }

        match &mut self.popup {
            Some(popup) => {
                popup.set_results(context.query, context.token_range, matches);
            }
            None => {
                self.popup = Some(CompletionPopup::new(
                    context.query,
                    context.token_range,
                    matches,
                ));
            }
        }
        self.dismissed_token = None;
        true
    }

    pub(crate) fn handle_key_event(
        &mut self,
        key_event: KeyEvent,
        composer: &mut ChatComposer,
    ) -> Option<InputResult> {
        if !self.enabled || matches!(key_event.kind, KeyEventKind::Release) || self.popup.is_none()
        {
            return None;
        }

        match key_event {
            KeyEvent {
                code: KeyCode::BackTab,
                ..
            }
            | KeyEvent {
                code: KeyCode::Tab,
                modifiers: KeyModifiers::SHIFT,
                ..
            } => {
                if let Some(popup) = &mut self.popup {
                    popup.move_previous();
                }
                Some(InputResult::None)
            }
            KeyEvent {
                code: KeyCode::Tab, ..
            }
            | KeyEvent {
                code: KeyCode::Down,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('n'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                if let Some(popup) = &mut self.popup {
                    popup.move_next();
                }
                Some(InputResult::None)
            }
            KeyEvent {
                code: KeyCode::Up, ..
            }
            | KeyEvent {
                code: KeyCode::Char('p'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                if let Some(popup) = &mut self.popup {
                    popup.move_previous();
                }
                Some(InputResult::None)
            }
            KeyEvent {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                let selected = self.popup.as_ref().and_then(CompletionPopup::selected);
                self.popup = None;
                if let Some((range, replacement)) = selected {
                    composer.replace_completion_token(range, replacement.as_str());
                    self.dismissed_token = Some(replacement);
                }
                Some(InputResult::None)
            }
            KeyEvent {
                code: KeyCode::Esc, ..
            } => {
                self.dismissed_token = self
                    .popup
                    .as_ref()
                    .map(|popup| popup.token_query().to_string());
                self.popup = None;
                Some(InputResult::None)
            }
            _ => None,
        }
    }

    pub(crate) fn ingest_history_cell(&mut self, cell: &dyn HistoryCell) -> bool {
        if !self.enabled {
            return false;
        }

        let mut ingested = false;
        for line in cell.raw_lines() {
            let text = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            self.index.ingest_text(&text);
            ingested = true;
        }
        ingested
    }

    pub(crate) fn overlay_buffer(
        &self,
        active_view: bool,
        cursor_x: u16,
        cursor_y: u16,
        screen_width: u16,
        screen_height: u16,
    ) -> Option<Buffer> {
        if !self.enabled || active_view || screen_width == 0 || screen_height == 0 {
            return None;
        }

        let popup = self.popup.as_ref()?;
        let bounds = Rect::new(0, 0, screen_width, screen_height);
        let width = popup.preferred_width().min(bounds.width);
        let height = popup.calculate_required_height().min(bounds.height);
        if width == 0 || height == 0 {
            return None;
        }

        let max_x = bounds.right().saturating_sub(width);
        let x = cursor_x.clamp(bounds.x, max_x.max(bounds.x));
        let min_y = bounds.y;
        let max_y = bounds.bottom().saturating_sub(height).max(bounds.y);
        let y = cursor_y.saturating_sub(height).clamp(min_y, max_y);
        let popup_rect = Rect::new(x, y, width, height);
        let mut overlay = Buffer::empty(popup_rect);
        popup.render_ref(popup_rect, &mut overlay);
        Some(overlay)
    }
}
