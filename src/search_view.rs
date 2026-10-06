//! Stateful Search view, independent of the parent app's current view.
//!
//! Call `open(true)` for Ctrl+F, `open(false)` for an activity/view switch,
//! and `close()` when leaving Search. Neither closing nor opening drops results.
//! Keep calling `tick` to collect workers, even when another view is displayed.
//! File/byte limits (20,000 files / 1 MiB) belong to `crate::search`; this view
//! requests at most 1,000 matching lines and never implements a second scanner.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use unicode_width::UnicodeWidthChar;

use crate::icons::IconTheme;
use crate::search::{
    ContentHit, SearchFocus, SearchOptions, collect_content_hits, highlighted_search_context,
};
use crate::{ACCENT, BG, BORDER, FG, MUTED, SELECTED};

const DEBOUNCE: Duration = Duration::from_millis(300);
const MATCH_LIMIT: usize = crate::search::CONTENT_SEARCH_MATCH_LIMIT;
// The error color is not part of main's shared palette.
const ERROR: Color = Color::Rgb(0xf0, 0x71, 0x78);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SearchAction {
    None,
    Close,
    OpenHit(PathBuf, usize),
}

#[derive(Clone, Copy, Default)]
struct SearchZones {
    query: Rect,
    replace: Rect,
    include: Rect,
    exclude: Rect,
    match_case: Rect,
    whole_word: Rect,
    regex: Rect,
    refresh: Rect,
    clear: Rect,
    details: Rect,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SearchRequest {
    root: PathBuf,
    show_hidden: bool,
    query: String,
    include: String,
    exclude: String,
    options: SearchOptions,
    // Reject A -> B -> A completions as well as ordinary stale inputs.
    revision: u64,
}

struct SearchWorker {
    request: SearchRequest,
    receiver: Receiver<Result<(Vec<ContentHit>, bool), String>>,
}

#[derive(Default)]
pub struct SearchView {
    active: bool,
    query: String,
    replace: String,
    include: String,
    exclude: String,
    options: SearchOptions,
    focus: SearchFocus,
    details_expanded: bool,
    results: Vec<ContentHit>,
    selected: usize,
    scroll: usize,
    snap: bool,
    truncated: bool,
    loading: bool,
    searched: bool,
    error: Option<String>,
    dirty_since: Option<Instant>,
    immediate: bool,
    revision: u64,
    environment: Option<(PathBuf, bool)>,
    worker: Option<SearchWorker>,
    zones: SearchZones,
    result_rows: Vec<(Rect, usize)>,
}

impl SearchView {
    pub fn new() -> Self {
        Self::default()
    }

    /// Re-enter Search without clearing its inputs, results, or live worker.
    /// Ctrl+F always focuses the query; an already-active activity switch keeps focus.
    pub fn open(&mut self, focus_query: bool) {
        if focus_query {
            self.focus = SearchFocus::Query;
        } else if !self.active {
            self.focus = SearchFocus::Results;
        }
        self.active = true;
    }

    /// Deactivate without discarding search state; also invalidates mouse zones.
    pub fn close(&mut self) {
        self.active = false;
        self.zones = SearchZones::default();
        self.result_rows.clear();
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Bare 1/2/3 may switch views only when no text input owns the keyboard.
    pub fn text_unfocused(&self) -> bool {
        self.active && self.focus == SearchFocus::Results
    }

    fn mark_dirty(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        self.results.clear();
        self.result_rows.clear();
        self.selected = 0;
        self.scroll = 0;
        self.snap = false;
        self.truncated = false;
        self.loading = false;
        self.searched = false;
        self.error = None;
        self.immediate = false;
        self.dirty_since = (!self.query.trim().is_empty()).then(Instant::now);
    }

    fn toggle_details(&mut self) {
        self.details_expanded = !self.details_expanded;
        if !self.details_expanded
            && matches!(self.focus, SearchFocus::Include | SearchFocus::Exclude)
        {
            self.focus = SearchFocus::Query;
        }
        // Hidden filter boxes must cease accepting clicks before the next draw.
        self.zones.include = Rect::default();
        self.zones.exclude = Rect::default();
    }

    fn request_search(&mut self) {
        if self.query.trim().is_empty() {
            return;
        }
        self.immediate = true;
        self.dirty_since = Some(Instant::now());
        self.start_search();
    }

    fn request(&self) -> Option<SearchRequest> {
        let (root, show_hidden) = self.environment.as_ref()?;
        Some(SearchRequest {
            root: root.clone(),
            show_hidden: *show_hidden,
            query: self.query.clone(),
            include: self.include.clone(),
            exclude: self.exclude.clone(),
            options: self.options,
            revision: self.revision,
        })
    }

    fn start_search(&mut self) {
        if self.worker.is_some() || self.query.trim().is_empty() {
            return;
        }
        let Some(request) = self.request() else {
            return;
        };
        self.results.clear();
        self.result_rows.clear();
        self.selected = 0;
        self.scroll = 0;
        self.snap = true;
        self.truncated = false;
        self.loading = true;
        self.searched = true;
        self.error = None;
        self.dirty_since = None;
        self.immediate = false;
        let (sender, receiver) = mpsc::channel();
        let work = request.clone();
        std::thread::spawn(move || {
            let result = collect_content_hits(
                &work.root,
                work.show_hidden,
                &work.query,
                &work.include,
                &work.exclude,
                work.options,
                MATCH_LIMIT,
            );
            let _ = sender.send(result);
        });
        // Keep a single worker even after inputs change, bounding background work.
        self.worker = Some(SearchWorker { request, receiver });
    }

    /// Update the workspace identity, collect a validated worker, then debounce.
    pub fn tick(&mut self, root: &Path, show_hidden: bool) {
        let environment_changed = self
            .environment
            .as_ref()
            .is_some_and(|(old_root, old_hidden)| old_root != root || *old_hidden != show_hidden);
        self.environment = Some((root.to_path_buf(), show_hidden));
        if environment_changed {
            self.mark_dirty();
        }
        let completion = self
            .worker
            .as_ref()
            .map(|worker| worker.receiver.try_recv());
        match completion {
            Some(Ok(result)) => {
                let worker = self.worker.take().expect("completed worker exists");
                if self.request().as_ref() == Some(&worker.request) {
                    self.loading = false;
                    self.searched = true;
                    self.dirty_since = None;
                    self.immediate = false;
                    match result {
                        Ok((results, truncated)) => {
                            self.results = results;
                            self.truncated = truncated;
                            self.error = None;
                        }
                        Err(error) => {
                            self.results.clear();
                            self.truncated = false;
                            self.error = Some(error);
                        }
                    }
                    self.selected = 0;
                    self.snap = true;
                }
            }
            Some(Err(TryRecvError::Disconnected)) => {
                let worker = self.worker.take().expect("disconnected worker exists");
                if self.request().as_ref() == Some(&worker.request) {
                    self.loading = false;
                    self.searched = true;
                    self.error = Some("Search worker stopped unexpectedly".to_string());
                    self.dirty_since = None;
                    self.immediate = false;
                }
            }
            Some(Err(TryRecvError::Empty)) | None => {}
        }
        if self.active
            && self
                .dirty_since
                .is_some_and(|since| self.immediate || since.elapsed() >= DEBOUNCE)
        {
            self.start_search();
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> SearchAction {
        if !self.active || key.kind != KeyEventKind::Press {
            return SearchAction::None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        if ctrl && !alt && matches!(key.code, KeyCode::Char('f' | 'F')) {
            self.focus = SearchFocus::Query;
            return SearchAction::None;
        }
        if alt && !ctrl {
            let toggled = match key.code {
                KeyCode::Char('c' | 'C') => {
                    self.options.match_case = !self.options.match_case;
                    true
                }
                KeyCode::Char('w' | 'W') => {
                    self.options.whole_word = !self.options.whole_word;
                    true
                }
                KeyCode::Char('r' | 'R') => {
                    self.options.regex = !self.options.regex;
                    true
                }
                _ => false,
            };
            if toggled {
                self.mark_dirty();
            }
            return SearchAction::None;
        }
        if ctrl
            && key.modifiers.contains(KeyModifiers::SHIFT)
            && matches!(key.code, KeyCode::Char('j' | 'J'))
        {
            self.toggle_details();
            return SearchAction::None;
        }
        if self.focus == SearchFocus::Results {
            self.snap = true;
        }
        match key.code {
            KeyCode::Esc => {
                self.close();
                return SearchAction::Close;
            }
            KeyCode::Tab => {
                self.focus = match self.focus {
                    SearchFocus::Query => SearchFocus::Replace,
                    SearchFocus::Replace if self.details_expanded => SearchFocus::Include,
                    SearchFocus::Replace => SearchFocus::Results,
                    SearchFocus::Include => SearchFocus::Exclude,
                    SearchFocus::Exclude => SearchFocus::Results,
                    SearchFocus::Results => SearchFocus::Query,
                };
            }
            KeyCode::BackTab => {
                self.focus = match self.focus {
                    SearchFocus::Query => SearchFocus::Results,
                    SearchFocus::Replace => SearchFocus::Query,
                    SearchFocus::Include => SearchFocus::Replace,
                    SearchFocus::Exclude => SearchFocus::Include,
                    SearchFocus::Results if self.details_expanded => SearchFocus::Exclude,
                    SearchFocus::Results => SearchFocus::Replace,
                };
            }
            KeyCode::Up if self.focus == SearchFocus::Results => {
                self.selected = self.selected.saturating_sub(1);
            }
            KeyCode::Down if self.focus == SearchFocus::Results => {
                self.selected = self
                    .selected
                    .saturating_add(1)
                    .min(self.results.len().saturating_sub(1));
            }
            KeyCode::Down if self.focus != SearchFocus::Replace && !self.results.is_empty() => {
                self.focus = SearchFocus::Results;
            }
            KeyCode::Enter if self.focus == SearchFocus::Results => {
                return self.open_selected();
            }
            KeyCode::Enter
                if matches!(
                    self.focus,
                    SearchFocus::Query | SearchFocus::Include | SearchFocus::Exclude
                ) && !self.loading =>
            {
                self.request_search();
            }
            KeyCode::Backspace => {
                let field = match self.focus {
                    SearchFocus::Query => &mut self.query,
                    SearchFocus::Replace => &mut self.replace,
                    SearchFocus::Include => &mut self.include,
                    SearchFocus::Exclude => &mut self.exclude,
                    SearchFocus::Results => return SearchAction::None,
                };
                field.pop();
                if self.focus != SearchFocus::Replace {
                    self.mark_dirty();
                }
            }
            KeyCode::Char(character) if !ctrl || alt => {
                let field = match self.focus {
                    SearchFocus::Query => &mut self.query,
                    SearchFocus::Replace => &mut self.replace,
                    SearchFocus::Include => &mut self.include,
                    SearchFocus::Exclude => &mut self.exclude,
                    SearchFocus::Results => return SearchAction::None,
                };
                field.push(character);
                if self.focus != SearchFocus::Replace {
                    self.mark_dirty();
                }
            }
            _ => {}
        }
        SearchAction::None
    }

    fn open_selected(&self) -> SearchAction {
        self.results
            .get(self.selected)
            .map(|hit| SearchAction::OpenHit(hit.path.clone(), hit.line))
            .unwrap_or(SearchAction::None)
    }

    pub fn on_mouse(&mut self, mouse: MouseEvent) -> SearchAction {
        if !self.active {
            return SearchAction::None;
        }
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.snap = false;
                self.scroll = self.scroll.saturating_sub(3);
            }
            MouseEventKind::ScrollDown => {
                self.snap = false;
                self.scroll = self.scroll.saturating_add(3);
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let contains = |zone| hit(zone, mouse.column, mouse.row);
                if contains(self.zones.refresh) {
                    self.request_search();
                } else if contains(self.zones.clear) {
                    self.query.clear();
                    self.replace.clear();
                    self.include.clear();
                    self.exclude.clear();
                    self.mark_dirty();
                    self.focus = SearchFocus::Query;
                } else if contains(self.zones.details) {
                    self.toggle_details();
                } else if contains(self.zones.match_case) {
                    self.options.match_case = !self.options.match_case;
                    self.mark_dirty();
                } else if contains(self.zones.whole_word) {
                    self.options.whole_word = !self.options.whole_word;
                    self.mark_dirty();
                } else if contains(self.zones.regex) {
                    self.options.regex = !self.options.regex;
                    self.mark_dirty();
                } else if contains(self.zones.query) {
                    self.focus = SearchFocus::Query;
                } else if contains(self.zones.replace) {
                    self.focus = SearchFocus::Replace;
                } else if self.details_expanded && contains(self.zones.include) {
                    self.focus = SearchFocus::Include;
                } else if self.details_expanded && contains(self.zones.exclude) {
                    self.focus = SearchFocus::Exclude;
                } else if let Some((_, index)) =
                    self.result_rows.iter().find(|(area, _)| contains(*area))
                {
                    self.selected = *index;
                    self.focus = SearchFocus::Results;
                    return self.open_selected();
                }
            }
            _ => {}
        }
        SearchAction::None
    }

    pub fn draw(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        theme: IconTheme,
        mouse_pos: Option<(u16, u16)>,
    ) {
        self.zones = SearchZones::default();
        self.result_rows.clear();
        let area = area.intersection(frame.area());
        if !self.active || area.is_empty() {
            return;
        }
        frame.render_widget(Block::default().style(Style::default().bg(BG).fg(FG)), area);
        let title = Rect::new(area.x, area.y, area.width, 1);
        frame.render_widget(
            Paragraph::new(" Search").style(Style::default().add_modifier(Modifier::BOLD)),
            title,
        );
        let toolbar_width = 9.min(area.width);
        let toolbar_x = area.x + area.width.saturating_sub(toolbar_width);
        self.zones.refresh = Rect::new(toolbar_x, title.y, 3, 1).intersection(area);
        self.zones.clear = Rect::new(toolbar_x.saturating_add(3), title.y, 3, 1).intersection(area);
        self.zones.details =
            Rect::new(toolbar_x.saturating_add(6), title.y, 3, 1).intersection(area);
        let hovered = |rect| mouse_pos.is_some_and(|(x, y)| hit(rect, x, y));
        let (refresh, clear, details) = search_toolbar_icons(theme);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    format!(" {refresh} "),
                    button_style(hovered(self.zones.refresh)),
                ),
                Span::styled(
                    format!(" {clear} "),
                    button_style(hovered(self.zones.clear)),
                ),
                Span::styled(
                    format!(" {details} "),
                    button_style(hovered(self.zones.details)),
                ),
            ]))
            .alignment(Alignment::Right),
            Rect::new(toolbar_x, title.y, toolbar_width, 1),
        );
        let query_box = Rect::new(
            area.x.saturating_add(2),
            area.y.saturating_add(2),
            area.width.saturating_sub(3),
            3,
        );
        let option_width = query_box.width.min(9);
        let option_x = query_box.x
            + query_box
                .width
                .saturating_sub(option_width.saturating_add(1));
        frame.render_widget(
            Block::bordered().border_style(input_border(self.focus == SearchFocus::Query)),
            query_box.intersection(area),
        );
        let query_inner = Rect::new(
            query_box.x.saturating_add(1),
            query_box.y.saturating_add(1),
            query_box
                .width
                .saturating_sub(option_width.saturating_add(2)),
            1,
        )
        .intersection(area);
        self.zones.query = query_inner;
        frame.render_widget(
            Paragraph::new(search_input_line(
                &self.query,
                "Search",
                self.focus == SearchFocus::Query,
                query_inner.width,
            )),
            query_inner,
        );
        let options_area =
            Rect::new(option_x, query_box.y.saturating_add(1), option_width, 1).intersection(area);
        let option_style = |active| {
            if active {
                selection_style(true)
            } else {
                muted_style()
            }
        };
        let option_spans = [
            Span::styled("Aa ", option_style(self.options.match_case)),
            Span::styled("ab ", option_style(self.options.whole_word)),
            Span::styled(".* ", option_style(self.options.regex)),
        ];
        self.zones.match_case =
            Rect::new(option_x, options_area.y, 3, 1).intersection(options_area);
        self.zones.whole_word =
            Rect::new(option_x.saturating_add(3), options_area.y, 3, 1).intersection(options_area);
        self.zones.regex =
            Rect::new(option_x.saturating_add(6), options_area.y, 3, 1).intersection(options_area);
        frame.render_widget(
            Paragraph::new(Line::from(option_spans.to_vec())),
            options_area,
        );
        let replace_box = Rect::new(
            query_box.x,
            query_box.y.saturating_add(query_box.height),
            query_box.width,
            3,
        );
        self.zones.replace = draw_field(
            frame,
            area,
            replace_box,
            &self.replace,
            "Replace",
            self.focus == SearchFocus::Replace,
        );
        let mut results_y = replace_box.bottom().saturating_add(1);
        if self.details_expanded {
            let label = Rect::new(query_box.x, results_y, query_box.width, 1).intersection(area);
            frame.render_widget(
                Paragraph::new("files to include").style(muted_style()),
                label,
            );
            let include_box =
                Rect::new(query_box.x, results_y.saturating_add(1), query_box.width, 3);
            self.zones.include = draw_field(
                frame,
                area,
                include_box,
                &self.include,
                "e.g. *.ts, src/**/include",
                self.focus == SearchFocus::Include,
            );
            let exclude_y = include_box.bottom().saturating_add(1);
            let label = Rect::new(query_box.x, exclude_y, query_box.width, 1).intersection(area);
            frame.render_widget(
                Paragraph::new("files to exclude").style(muted_style()),
                label,
            );
            let exclude_box =
                Rect::new(query_box.x, exclude_y.saturating_add(1), query_box.width, 3);
            self.zones.exclude = draw_field(
                frame,
                area,
                exclude_box,
                &self.exclude,
                "e.g. node_modules, **/*.min.js",
                self.focus == SearchFocus::Exclude,
            );
            results_y = exclude_box.bottom().saturating_add(1);
        }
        if results_y >= area.bottom() {
            return;
        }
        let status = if let Some(error) = &self.error {
            Some(error.clone())
        } else if self.loading {
            Some("Searching…".to_string())
        } else if !self.searched {
            None
        } else if self.results.is_empty() {
            Some("No results found".to_string())
        } else if self.truncated {
            Some(format!(
                "{} results · result limit reached",
                self.results.len()
            ))
        } else {
            Some(format!("{} results", self.results.len()))
        };
        if let Some(status) = status {
            let style = if self.error.is_some() {
                Style::default().fg(ERROR)
            } else {
                muted_style()
            };
            frame.render_widget(
                Paragraph::new(status).style(style),
                Rect::new(
                    area.x.saturating_add(1),
                    results_y,
                    area.width.saturating_sub(2),
                    1,
                )
                .intersection(area),
            );
            results_y = results_y.saturating_add(1);
        }
        let list_area = Rect::new(
            area.x,
            results_y,
            area.width,
            area.bottom().saturating_sub(results_y),
        );
        let mut rows: Vec<(Option<usize>, Line<'static>)> = Vec::new();
        let mut hit_index = 0;
        while hit_index < self.results.len() {
            let label = &self.results[hit_index].label;
            let mut end = hit_index + 1;
            while end < self.results.len() && self.results[end].label == *label {
                end += 1;
            }
            let count_text = format!(" {}", end - hit_index);
            let label_width =
                usize::from(list_area.width).saturating_sub(2 + Span::raw(&count_text).width());
            rows.push((
                None,
                Line::from(vec![
                    Span::styled("⌄ ", muted_style()),
                    Span::styled(
                        truncate_path_tail(label, label_width),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(count_text, muted_style()),
                ]),
            ));
            for index in hit_index..end {
                let hit = &self.results[index];
                let prefix = format!("   {}: ", hit.line);
                let context_width =
                    usize::from(list_area.width).saturating_sub(Span::raw(&prefix).width());
                let mut spans = vec![Span::styled(prefix, Style::default().fg(ACCENT))];
                spans.extend(highlighted_search_context(
                    &hit.context,
                    &hit.matches,
                    context_width,
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ));
                rows.push((Some(index), Line::from(spans)));
            }
            hit_index = end;
        }
        let viewport = usize::from(list_area.height);
        if viewport == 0 {
            return;
        }
        let selected_row = rows
            .iter()
            .position(|(index, _)| *index == Some(self.selected))
            .unwrap_or(0);
        let max_start = rows.len().saturating_sub(viewport);
        let mut start = if self.snap {
            selected_row.saturating_sub(viewport / 2).min(max_start)
        } else {
            self.scroll.min(max_start)
        };
        if self.snap {
            while start > 0 && rows[start].0.is_some() {
                start -= 1;
            }
        }
        self.scroll = start;
        self.snap = false;
        for (offset, (index, line)) in rows.into_iter().skip(start).take(viewport).enumerate() {
            let row_area = Rect::new(list_area.x, list_area.y + offset as u16, list_area.width, 1);
            let style = if index == Some(self.selected) {
                selection_style(self.focus == SearchFocus::Results)
            } else {
                Style::default()
            };
            if let Some(index) = index {
                self.result_rows.push((row_area, index));
            }
            frame.render_widget(Paragraph::new(line).style(style), row_area);
        }
    }
}

fn hit(area: Rect, x: u16, y: u16) -> bool {
    x >= area.x && x < area.right() && y >= area.y && y < area.bottom()
}

fn muted_style() -> Style {
    Style::default().fg(MUTED)
}

fn input_border(focused: bool) -> Style {
    Style::default().fg(if focused { ACCENT } else { BORDER })
}

fn selection_style(focused: bool) -> Style {
    if focused {
        Style::default()
            .fg(FG)
            .bg(SELECTED)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(FG).bg(BORDER)
    }
}

fn button_style(hovered: bool) -> Style {
    if hovered {
        Style::default().fg(FG).bg(SELECTED)
    } else {
        muted_style()
    }
}

fn draw_field(
    frame: &mut Frame<'_>,
    area: Rect,
    bounds: Rect,
    value: &str,
    placeholder: &str,
    focused: bool,
) -> Rect {
    frame.render_widget(
        Block::bordered().border_style(input_border(focused)),
        bounds.intersection(area),
    );
    let inner = Rect::new(
        bounds.x.saturating_add(1),
        bounds.y.saturating_add(1),
        bounds.width.saturating_sub(2),
        1,
    )
    .intersection(area);
    frame.render_widget(
        Paragraph::new(search_input_line(value, placeholder, focused, inner.width)),
        inner,
    );
    inner
}

fn search_input_line(value: &str, placeholder: &str, focused: bool, width: u16) -> Line<'static> {
    let available = usize::from(width).saturating_sub(usize::from(focused));
    let mut spans = if value.is_empty() && !focused {
        // The shared context helper already performs Unicode-width truncation.
        highlighted_search_context(placeholder, &[], available, Style::default())
            .into_iter()
            .map(|span| span.style(muted_style()))
            .collect()
    } else if value.is_empty() {
        Vec::new()
    } else {
        vec![Span::raw(truncate_path_tail(value, available))]
    };
    if focused && width > 0 {
        spans.push(Span::raw("█"));
    }
    Line::from(spans)
}

fn search_toolbar_icons(theme: IconTheme) -> (&'static str, &'static str, &'static str) {
    match theme {
        IconTheme::Material => ("\u{eb37}", "\u{eabf}", "\u{ea7c}"),
        IconTheme::Emoji => ("⟳", "×", "⋯"),
    }
}

/// Inputs and paths both keep the tail, so typing and filenames remain visible.
fn truncate_path_tail(label: &str, max: usize) -> String {
    if Span::raw(label).width() <= max {
        return label.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut used = 1;
    let mut reversed = Vec::new();
    for character in label.chars().rev() {
        let width = character.width().unwrap_or(0);
        if used + width > max {
            break;
        }
        used += width;
        reversed.push(character);
    }
    format!("…{}", reversed.into_iter().rev().collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn key(view: &mut SearchView, code: KeyCode, modifiers: KeyModifiers) -> SearchAction {
        view.on_key(KeyEvent::new(code, modifiers))
    }

    fn press(view: &mut SearchView, code: KeyCode) -> SearchAction {
        key(view, code, KeyModifiers::NONE)
    }

    fn click(view: &mut SearchView, area: Rect) -> SearchAction {
        view.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: area.x,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn result(label: &str, line: usize) -> ContentHit {
        ContentHit {
            path: PathBuf::from(label),
            label: label.to_string(),
            line,
            context: "needle here".to_string(),
            matches: vec![(0, 6)],
        }
    }

    fn draw(view: &mut SearchView, width: u16, height: u16) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| view.draw(frame, frame.area(), IconTheme::Emoji, None))
            .unwrap();
        terminal
    }

    fn screen(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn view_switching_preserves_results_and_focus_controls_digits() {
        let mut view = SearchView::new();
        assert!(!view.is_active());
        assert!(!view.text_unfocused());
        view.open(true);
        press(&mut view, KeyCode::Char('3'));
        assert_eq!(view.query, "3");
        view.results = vec![result("src/main.rs", 12)];
        assert_eq!(press(&mut view, KeyCode::Esc), SearchAction::Close);
        assert!(!view.is_active());
        view.open(false);
        assert!(view.text_unfocused());
        assert_eq!(
            press(&mut view, KeyCode::Enter),
            SearchAction::OpenHit("src/main.rs".into(), 12)
        );
        assert_eq!(view.query, "3");
        key(&mut view, KeyCode::Char('f'), KeyModifiers::CONTROL);
        assert_eq!(view.focus, SearchFocus::Query);
        assert_eq!(
            key(&mut view, KeyCode::Char('q'), KeyModifiers::CONTROL),
            SearchAction::None
        );
        view.close();
        view.open(false);
        assert_eq!(press(&mut view, KeyCode::Char('q')), SearchAction::None);
        assert_eq!(view.query, "3");
        view.open(true);
        press(&mut view, KeyCode::Char('q'));
        assert_eq!(view.query, "3q");
    }

    #[test]
    fn opening_active_search_preserves_focus_unless_query_requested() {
        let mut view = SearchView::default();
        view.open(false);
        assert_eq!(view.focus, SearchFocus::Results);
        view.results = vec![result("a.rs", 7), result("b.rs", 9)];
        view.selected = 1;
        view.scroll = 3;
        for focus in [
            SearchFocus::Query,
            SearchFocus::Replace,
            SearchFocus::Include,
            SearchFocus::Exclude,
            SearchFocus::Results,
        ] {
            view.focus = focus;
            view.open(false);
            assert_eq!(view.focus, focus);
            view.open(true);
            assert_eq!(view.focus, SearchFocus::Query);
            assert_eq!(view.results.len(), 2);
            assert_eq!(view.selected, 1);
            assert_eq!(view.scroll, 3);
        }
        view.close();
        view.open(false);
        assert_eq!(view.focus, SearchFocus::Results);
        assert_eq!(view.results.len(), 2);
    }

    #[test]
    fn refresh_clears_previous_results_and_error_before_work_starts() {
        let mut view = SearchView::default();
        view.open(true);
        view.tick(Path::new("workspace"), false);
        press(&mut view, KeyCode::Char('x'));
        view.results = vec![result("old.rs", 7)];
        view.selected = 3;
        view.scroll = 4;
        view.truncated = true;
        view.error = Some("Previous error".into());
        draw(&mut view, 50, 20);
        let old_row = view.result_rows[0].0;
        let refresh = view.zones.refresh;
        click(&mut view, refresh);
        assert!(view.worker.is_some());
        assert!(view.results.is_empty());
        assert!(view.result_rows.is_empty());
        assert!(view.error.is_none());
        assert!(!view.truncated);
        assert_eq!(view.selected, 0);
        assert_eq!(view.scroll, 0);
        assert!(view.loading);
        assert!(view.searched);
        assert_eq!(click(&mut view, old_row), SearchAction::None);
        assert!(screen(&draw(&mut view, 50, 20)).contains("Searching…"));
    }

    #[test]
    fn tabs_filters_options_and_inert_replace_follow_upstream() {
        let mut view = SearchView::new();
        view.open(true);
        press(&mut view, KeyCode::Char('x'));
        view.results = vec![result("a", 1)];
        let revision = view.revision;
        press(&mut view, KeyCode::Tab);
        assert_eq!(view.focus, SearchFocus::Replace);
        press(&mut view, KeyCode::Char('y'));
        press(&mut view, KeyCode::Enter);
        assert_eq!(view.replace, "y");
        assert_eq!(view.results.len(), 1);
        assert_eq!(view.revision, revision);
        key(
            &mut view,
            KeyCode::Char('J'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        press(&mut view, KeyCode::Tab);
        assert_eq!(view.focus, SearchFocus::Include);
        press(&mut view, KeyCode::Char('*'));
        assert_eq!(view.include, "*");
        assert!(view.results.is_empty());
        assert!(view.dirty_since.is_some());
        press(&mut view, KeyCode::Tab);
        assert_eq!(view.focus, SearchFocus::Exclude);
        press(&mut view, KeyCode::Tab);
        assert_eq!(view.focus, SearchFocus::Results);
        press(&mut view, KeyCode::BackTab);
        assert_eq!(view.focus, SearchFocus::Exclude);
        key(
            &mut view,
            KeyCode::Char('j'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        assert_eq!(view.focus, SearchFocus::Query);
        for code in ['c', 'w', 'r'] {
            key(&mut view, KeyCode::Char(code), KeyModifiers::ALT);
        }
        assert_eq!(
            view.options,
            SearchOptions {
                match_case: true,
                whole_word: true,
                regex: true
            }
        );
        // Alt letters not used by the toggles are swallowed, not typed.
        key(&mut view, KeyCode::Char('a'), KeyModifiers::ALT);
        assert_eq!(view.query, "x");
    }

    fn queue_completion(
        view: &mut SearchView,
        request: SearchRequest,
        output: Result<(Vec<ContentHit>, bool), String>,
    ) {
        let (sender, receiver) = mpsc::channel();
        sender.send(output).unwrap();
        view.worker = Some(SearchWorker { request, receiver });
    }

    #[test]
    fn worker_validates_all_inputs_and_accepts_results_when_closed() {
        let root = Path::new("workspace");
        let mut view = SearchView::new();
        view.open(true);
        view.tick(root, false);
        press(&mut view, KeyCode::Char('x'));
        let original = view.request().unwrap();
        for mismatch in 0..7 {
            let mut request = original.clone();
            match mismatch {
                0 => request.root = "other".into(),
                1 => request.show_hidden = true,
                2 => request.query.push('y'),
                3 => request.include.push('*'),
                4 => request.exclude.push('*'),
                5 => request.options.regex = true,
                _ => request.revision = request.revision.wrapping_sub(1),
            }
            queue_completion(&mut view, request, Ok((vec![result("stale", 1)], false)));
            view.close();
            view.tick(root, false);
            assert!(view.results.is_empty());
        }
        queue_completion(&mut view, original, Ok((vec![result("current", 2)], true)));
        view.tick(root, false);
        assert_eq!(view.results[0].label, "current");
        assert!(view.truncated);
        assert!(!view.loading);
        view.open(false);
        assert_eq!(view.results.len(), 1);
        view.tick(root, true);
        assert!(view.results.is_empty());
        assert!(view.dirty_since.is_some());
    }

    #[test]
    fn worker_error_disconnect_and_clear_do_not_restore_stale_results() {
        let root = Path::new("workspace");
        let mut view = SearchView::new();
        view.open(true);
        view.tick(root, false);
        press(&mut view, KeyCode::Char('x'));
        let request = view.request().unwrap();
        queue_completion(
            &mut view,
            request.clone(),
            Err("Invalid include pattern".into()),
        );
        view.loading = true;
        view.tick(root, false);
        assert_eq!(view.error.as_deref(), Some("Invalid include pattern"));
        assert!(!view.loading);
        let (sender, receiver) = mpsc::channel();
        drop(sender);
        view.worker = Some(SearchWorker {
            request: request.clone(),
            receiver,
        });
        view.loading = true;
        view.tick(root, false);
        assert_eq!(
            view.error.as_deref(),
            Some("Search worker stopped unexpectedly")
        );
        assert!(!view.loading);
        queue_completion(&mut view, request, Ok((vec![result("stale", 1)], false)));
        draw(&mut view, 50, 20);
        let clear = view.zones.clear;
        click(&mut view, clear);
        view.tick(root, false);
        assert!(view.results.is_empty());
        assert!(view.error.is_none());
        assert!(!view.loading);
        assert!(view.worker.is_none());
        assert!(view.dirty_since.is_none());
    }

    #[test]
    fn debounce_and_blank_query_are_bounded() {
        let mut view = SearchView::new();
        let root = Path::new("workspace-that-does-not-exist");
        view.open(true);
        view.tick(root, false);
        press(&mut view, KeyCode::Char(' '));
        assert!(view.dirty_since.is_none());
        press(&mut view, KeyCode::Char('x'));
        view.tick(root, false);
        assert!(view.worker.is_none());
        view.dirty_since = Some(Instant::now() - DEBOUNCE);
        view.tick(root, false);
        assert!(view.worker.is_some());
        assert!(view.loading);
        assert!(view.dirty_since.is_none());
        // Editing never starts a parallel scanner while the old one is live.
        press(&mut view, KeyCode::Char('y'));
        let original = view.worker.as_ref().unwrap().request.clone();
        press(&mut view, KeyCode::Enter);
        assert_eq!(view.worker.as_ref().unwrap().request, original);
        assert!(view.immediate);
    }

    #[test]
    fn draw_hitboxes_grouping_highlights_scroll_and_clear() {
        let mut view = SearchView::new();
        view.open(false);
        view.query = "needle".into();
        view.searched = true;
        view.results = vec![result("a.rs", 10), result("a.rs", 11), result("b.rs", 20)];
        let terminal = draw(&mut view, 50, 20);
        let text = screen(&terminal);
        assert!(text.contains("3 results"));
        assert!(text.contains("⌄ a.rs 2"));
        assert!(text.contains("⌄ b.rs 1"));
        assert_eq!(view.result_rows.len(), 3);
        let hit_area = view.result_rows[1].0;
        assert_eq!(
            click(&mut view, hit_area),
            SearchAction::OpenHit("a.rs".into(), 11)
        );
        let highlighted = terminal.backend().buffer().cell((7, hit_area.y)).unwrap();
        assert_eq!(highlighted.fg, ACCENT);
        assert!(highlighted.modifier.contains(Modifier::BOLD));
        let selected = view.selected;
        view.on_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 1,
            row: 12,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(view.scroll, 3);
        assert_eq!(view.selected, selected);
        assert!(!view.snap);
        let details = view.zones.details;
        click(&mut view, details);
        assert!(view.details_expanded);
        draw(&mut view, 50, 30);
        let include = view.zones.include;
        click(&mut view, include);
        assert_eq!(view.focus, SearchFocus::Include);
        let clear = view.zones.clear;
        click(&mut view, clear);
        assert!(view.query.is_empty());
        assert!(view.results.is_empty());
        assert_eq!(view.focus, SearchFocus::Query);
        assert!(!view.searched);
    }

    #[test]
    fn error_loading_empty_and_limit_statuses_render() {
        let mut view = SearchView::new();
        view.open(false);
        view.error = Some("Invalid regular expression".into());
        assert!(screen(&draw(&mut view, 60, 20)).contains("Invalid regular expression"));
        view.error = None;
        view.loading = true;
        assert!(screen(&draw(&mut view, 60, 20)).contains("Searching…"));
        view.loading = false;
        view.searched = true;
        assert!(screen(&draw(&mut view, 60, 20)).contains("No results found"));
        view.results = vec![result("a", 1)];
        view.truncated = true;
        assert!(screen(&draw(&mut view, 60, 20)).contains("1 results · result limit reached"));
    }

    #[test]
    fn small_viewports_and_unicode_inputs_are_safe() {
        let mut view = SearchView::new();
        view.open(true);
        view.query = "very-long-界界needle".into();
        view.details_expanded = true;
        for width in 1..15 {
            for height in 1..22 {
                draw(&mut view, width, height);
            }
        }
        for width in 0..15 {
            let line = search_input_line(&view.query, "Search", true, width);
            assert!(line.width() <= usize::from(width));
        }
        assert_eq!(
            search_toolbar_icons(IconTheme::Material),
            ("\u{eb37}", "\u{eabf}", "\u{ea7c}")
        );
    }
}
