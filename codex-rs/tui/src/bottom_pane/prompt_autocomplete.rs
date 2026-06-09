use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread;

use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
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
use super::chat_composer::CompletionContext;
use super::completion_index::CompletionMatch;
use super::completion_index::SessionCompletionIndex;
use super::completion_popup::CompletionPopup;

pub(crate) struct PromptAutocomplete {
    search: PromptAutocompleteSearch,
    popup: Option<CompletionPopup>,
    dismissed_token: Option<String>,
    requested: Option<RequestedSearch>,
    enabled: bool,
    dictionary_enabled: bool,
}

impl PromptAutocomplete {
    pub(crate) fn new(app_event_tx: AppEventSender) -> Self {
        Self {
            search: PromptAutocompleteSearch::new(app_event_tx),
            popup: None,
            dismissed_token: None,
            requested: None,
            enabled: true,
            dictionary_enabled: true,
        }
    }

    pub(crate) fn set_config(
        &mut self,
        enabled: bool,
        dictionary_enabled: bool,
        composer: &ChatComposer,
        blocked: bool,
    ) -> bool {
        let dictionary_changed = self.dictionary_enabled != dictionary_enabled;
        self.enabled = enabled;
        self.dictionary_enabled = dictionary_enabled;
        if dictionary_changed {
            self.requested = None;
        }
        if !enabled {
            self.dismissed_token = None;
            self.requested = None;
            self.search.invalidate();
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
            self.requested = None;
            self.search.invalidate();
            return self.clear_popup();
        }

        if blocked {
            self.requested = None;
            self.search.invalidate();
            return self.clear_popup();
        }

        let Some(context) = composer.completion_context() else {
            self.dismissed_token = None;
            self.requested = None;
            self.search.invalidate();
            return self.clear_popup();
        };

        if self.dismissed_token.as_ref() == Some(&context.query) {
            self.requested = None;
            self.search.invalidate();
            return self.clear_popup();
        }

        let key = SearchKey::new(&context, self.dictionary_enabled);
        if self
            .requested
            .as_ref()
            .is_some_and(|requested| requested.key == key)
        {
            return false;
        }

        let request_id = self.search.search(key.clone());
        self.requested = Some(RequestedSearch { request_id, key });
        self.dismissed_token = None;
        false
    }

    pub(crate) fn on_search_result(
        &mut self,
        result: PromptAutocompleteResult,
        composer: &ChatComposer,
        blocked: bool,
    ) -> bool {
        if !self.enabled || blocked {
            return self.clear_popup();
        }

        let Some(requested) = self.requested.as_ref() else {
            return false;
        };
        let key = result.key();
        if requested.request_id != result.request_id || requested.key != key {
            return false;
        }

        let Some(context) = composer.completion_context() else {
            return false;
        };
        if SearchKey::new(&context, self.dictionary_enabled) != key
            || self.dismissed_token.as_ref() == Some(&context.query)
        {
            return false;
        }

        if result.matches.is_empty() {
            return self.clear_popup();
        }

        match &mut self.popup {
            Some(popup) => {
                popup.set_results(result.query, result.token_range, result.matches);
            }
            None => {
                self.popup = Some(CompletionPopup::new(
                    result.query,
                    result.token_range,
                    result.matches,
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
                let popup_matches_context = self.popup.as_ref().is_some_and(|popup| {
                    composer
                        .completion_context()
                        .is_some_and(|context| popup.matches_context(&context))
                });
                if !popup_matches_context {
                    return Some(InputResult::None);
                }

                let selected = self.popup.as_ref().and_then(CompletionPopup::selected);
                self.popup = None;
                if let Some((range, replacement)) = selected {
                    composer.replace_completion_token(range, replacement.as_str());
                    self.dismissed_token = Some(replacement);
                }
                self.requested = None;
                self.search.invalidate();
                Some(InputResult::None)
            }
            KeyEvent {
                code: KeyCode::Esc, ..
            } => {
                self.dismissed_token = composer
                    .completion_context()
                    .map(|context| context.query)
                    .or_else(|| {
                        self.popup
                            .as_ref()
                            .map(|popup| popup.token_query().to_string())
                    });
                self.popup = None;
                self.requested = None;
                self.search.invalidate();
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
        let mut lines = Vec::new();
        for line in cell.raw_lines() {
            let text = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            lines.push(text);
            ingested = true;
        }
        if ingested {
            self.requested = None;
            self.search.ingest_texts(lines);
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
        let y = cursor_y
            .saturating_sub(height)
            .saturating_sub(1)
            .clamp(min_y, max_y);
        let popup_rect = Rect::new(x, y, width, height);
        let mut overlay = Buffer::empty(popup_rect);
        popup.render_ref(popup_rect, &mut overlay);
        Some(overlay)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RequestedSearch {
    request_id: u64,
    key: SearchKey,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SearchKey {
    query: String,
    token_range: Range<usize>,
    dictionary_enabled: bool,
}

impl SearchKey {
    fn new(context: &CompletionContext, dictionary_enabled: bool) -> Self {
        Self {
            query: context.query.clone(),
            token_range: context.token_range.clone(),
            dictionary_enabled,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PromptAutocompleteResult {
    pub(crate) request_id: u64,
    pub(crate) query: String,
    pub(crate) token_range: Range<usize>,
    pub(crate) dictionary_enabled: bool,
    pub(crate) matches: Vec<CompletionMatch>,
}

impl PromptAutocompleteResult {
    fn key(&self) -> SearchKey {
        SearchKey {
            query: self.query.clone(),
            token_range: self.token_range.clone(),
            dictionary_enabled: self.dictionary_enabled,
        }
    }
}

struct PromptAutocompleteSearch {
    tx: Option<mpsc::Sender<PromptAutocompleteWorkerMessage>>,
    latest_request_id: Arc<AtomicU64>,
    worker: Option<thread::JoinHandle<()>>,
}

impl PromptAutocompleteSearch {
    fn new(app_event_tx: AppEventSender) -> Self {
        let (tx, rx) = mpsc::channel();
        let latest_request_id = Arc::new(AtomicU64::new(0));
        let worker_latest_request_id = latest_request_id.clone();
        let worker = match thread::Builder::new()
            .name("prompt-autocomplete".to_string())
            .spawn(move || {
                run_prompt_autocomplete_worker(rx, worker_latest_request_id, app_event_tx);
            }) {
            Ok(worker) => Some(worker),
            Err(err) => {
                tracing::warn!("failed to start prompt autocomplete worker: {err}");
                None
            }
        };
        Self {
            tx: Some(tx),
            latest_request_id,
            worker,
        }
    }

    fn search(&self, key: SearchKey) -> u64 {
        let request_id = self
            .latest_request_id
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        let Some(tx) = self.tx.as_ref() else {
            tracing::warn!("prompt autocomplete worker is unavailable");
            return request_id;
        };
        if tx
            .send(PromptAutocompleteWorkerMessage::Search(
                PromptAutocompleteSearchRequest { request_id, key },
            ))
            .is_err()
        {
            tracing::warn!("prompt autocomplete worker is unavailable");
        }
        request_id
    }

    fn ingest_texts(&self, lines: Vec<String>) {
        let Some(tx) = self.tx.as_ref() else {
            tracing::warn!("prompt autocomplete worker is unavailable");
            return;
        };
        if tx
            .send(PromptAutocompleteWorkerMessage::Ingest(lines))
            .is_err()
        {
            tracing::warn!("prompt autocomplete worker is unavailable");
        }
    }

    fn invalidate(&self) {
        self.latest_request_id.fetch_add(1, Ordering::AcqRel);
    }
}

impl Drop for PromptAutocompleteSearch {
    fn drop(&mut self) {
        self.invalidate();
        self.tx.take();
        if let Some(worker) = self.worker.take()
            && let Err(err) = worker.join()
        {
            tracing::warn!("prompt autocomplete worker panicked: {err:?}");
        }
    }
}

#[derive(Debug)]
enum PromptAutocompleteWorkerMessage {
    Ingest(Vec<String>),
    Search(PromptAutocompleteSearchRequest),
}

#[derive(Clone, Debug)]
struct PromptAutocompleteSearchRequest {
    request_id: u64,
    key: SearchKey,
}

fn run_prompt_autocomplete_worker(
    rx: mpsc::Receiver<PromptAutocompleteWorkerMessage>,
    latest_request_id: Arc<AtomicU64>,
    app_event_tx: AppEventSender,
) {
    let mut index = SessionCompletionIndex::default();
    while let Ok(message) = rx.recv() {
        match message {
            PromptAutocompleteWorkerMessage::Ingest(lines) => {
                ingest_lines(&mut index, lines);
            }
            PromptAutocompleteWorkerMessage::Search(request) => {
                let Some(request) = drain_pending_worker_messages(&rx, &mut index, request) else {
                    return;
                };
                run_prompt_autocomplete_search(&index, request, &latest_request_id, &app_event_tx);
            }
        }
    }
}

fn drain_pending_worker_messages(
    rx: &mpsc::Receiver<PromptAutocompleteWorkerMessage>,
    index: &mut SessionCompletionIndex,
    mut request: PromptAutocompleteSearchRequest,
) -> Option<PromptAutocompleteSearchRequest> {
    loop {
        match rx.try_recv() {
            Ok(PromptAutocompleteWorkerMessage::Ingest(lines)) => {
                ingest_lines(index, lines);
            }
            Ok(PromptAutocompleteWorkerMessage::Search(next_request)) => {
                request = next_request;
            }
            Err(mpsc::TryRecvError::Empty) => return Some(request),
            Err(mpsc::TryRecvError::Disconnected) => return None,
        }
    }
}

fn ingest_lines(index: &mut SessionCompletionIndex, lines: Vec<String>) {
    for line in lines {
        index.ingest_text(&line);
    }
}

fn run_prompt_autocomplete_search(
    index: &SessionCompletionIndex,
    request: PromptAutocompleteSearchRequest,
    latest_request_id: &AtomicU64,
    app_event_tx: &AppEventSender,
) {
    if latest_request_id.load(Ordering::Acquire) != request.request_id {
        return;
    }

    let is_cancelled = || latest_request_id.load(Ordering::Acquire) != request.request_id;
    let Some(matches) = index.search_with_dictionary_cancellable(
        &request.key.query,
        request.key.dictionary_enabled,
        &is_cancelled,
    ) else {
        return;
    };
    if matches.is_empty() {
        return;
    }

    if latest_request_id.load(Ordering::Acquire) != request.request_id {
        return;
    }

    app_event_tx.send(AppEvent::PromptAutocompleteResult(
        PromptAutocompleteResult {
            request_id: request.request_id,
            query: request.key.query,
            token_range: request.key.token_range,
            dictionary_enabled: request.key.dictionary_enabled,
            matches,
        },
    ));
}
