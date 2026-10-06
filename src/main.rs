mod icons;
mod tree;

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use tree::{Row, Tree};

const BG: Color = Color::Rgb(0x1b, 0x1d, 0x29);
const FG: Color = Color::Rgb(0xd2, 0xd6, 0xe0);
const MUTED: Color = Color::Rgb(0x78, 0x82, 0x9b);
const ACCENT: Color = Color::Rgb(0x82, 0xaa, 0xff);
const BORDER: Color = Color::Rgb(0x3d, 0x4a, 0x70);
const SELECTED: Color = Color::Rgb(0x36, 0x42, 0x62);
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(300);
// ponytail: these caps bound recursive search cost; add cancellation/progress if larger trees need it.
const SEARCH_MAX_FILES: usize = 20_000;
const SEARCH_MAX_BYTES: u64 = 1024 * 1024;
const SEARCH_MAX_HITS: usize = 300;
const GRAPH_MAX_COMMITS: usize = 200;

fn main() -> io::Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("-h" | "--help") => {
            println!("usage: teditor [directory-or-file]");
            return Ok(());
        }
        Some("--version") => {
            println!("teditor {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        _ => {}
    }
    let (root, selected) = parse_args()?;
    let mut app = App::new(root, selected);
    let mut terminal = ratatui::init();
    let result = (|| {
        crossterm::execute!(io::stdout(), EnableMouseCapture)?;
        run(&mut terminal, &mut app)
    })();
    let _ = crossterm::execute!(io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

fn parse_args() -> io::Result<(PathBuf, Option<PathBuf>)> {
    let mut args = std::env::args_os().skip(1);
    let Some(arg) = args.next() else {
        return Ok((std::env::current_dir()?, None));
    };
    if args.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: teditor [directory-or-file]",
        ));
    }

    let path = std::fs::canonicalize(PathBuf::from(arg))?;
    if path.is_dir() {
        Ok((path, None))
    } else if path.is_file() {
        let root = path.parent().unwrap_or(Path::new(".")).to_path_buf();
        Ok((root, Some(path)))
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path must be a file or directory",
        ))
    }
}

fn run(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> io::Result<()> {
    loop {
        app.poll_workers();
        terminal.draw(|frame| app.draw(frame))?;
        if event::poll(Duration::from_millis(120))? && handle_event(app, event::read()?) {
            break;
        }
    }
    Ok(())
}

fn handle_event(app: &mut App, event: Event) -> bool {
    match event {
        Event::Key(key) => app.on_key(key),
        Event::Mouse(mouse) => {
            app.on_mouse(mouse);
            false
        }
        _ => false,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Explorer,
    Search,
    GitGraph,
}

impl View {
    fn index(self) -> usize {
        match self {
            Self::Explorer => 0,
            Self::Search => 1,
            Self::GitGraph => 2,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Explorer => "EXPLORER",
            Self::Search => "SEARCH",
            Self::GitGraph => "GIT GRAPH",
        }
    }
}

struct SearchHit {
    path: String,
    line: usize,
    context: String,
}

#[derive(Default)]
struct SearchOutput {
    hits: Vec<SearchHit>,
    truncated: bool,
}

struct SearchCompletion {
    query: String,
    output: SearchOutput,
}

struct App {
    tree: Tree,
    view: View,
    icon_theme: icons::IconTheme,
    status: Option<String>,

    explorer_selected: usize,
    explorer_scroll: usize,
    search_query: String,
    search_hits: Vec<SearchHit>,
    search_selected: usize,
    search_scroll: usize,
    search_focus: bool,
    search_truncated: bool,
    search_loading: bool,
    search_due: Option<Instant>,
    search_rx: Option<Receiver<SearchCompletion>>,
    graph_lines: Vec<String>,
    graph_selected: usize,
    graph_scroll: usize,
    graph_error: Option<String>,
    graph_loading: bool,
    graph_loaded: bool,
    graph_rx: Option<Receiver<Result<Vec<String>, String>>>,

    body: Rect,
    query_area: Rect,
    activity_buttons: [Rect; 3],
    header_actions: [Rect; 2],
}

impl App {
    fn new(root: PathBuf, selected_path: Option<PathBuf>) -> Self {
        let mut tree = Tree::new(root);
        let rows = tree.rows();
        let explorer_selected = selected_path
            .and_then(|path| rows.iter().position(|row| row.path == path))
            .unwrap_or(0);
        Self {
            tree,
            view: View::Explorer,
            icon_theme: icons::IconTheme::resolve(None, None),
            status: None,
            explorer_selected,
            explorer_scroll: 0,
            search_query: String::new(),
            search_hits: Vec::new(),
            search_selected: 0,
            search_scroll: 0,
            search_focus: true,
            search_truncated: false,
            search_loading: false,
            search_due: None,
            search_rx: None,
            graph_lines: Vec::new(),
            graph_selected: 0,
            graph_scroll: 0,
            graph_error: None,
            graph_loading: false,
            graph_loaded: false,
            graph_rx: None,
            body: Rect::default(),
            query_area: Rect::default(),
            activity_buttons: [Rect::default(); 3],
            header_actions: [Rect::default(); 2],
        }
    }

    fn set_view(&mut self, view: View) {
        self.view = view;
        self.status = None;
        if view == View::Search {
            self.search_focus = true;
        } else if view == View::GitGraph && !self.graph_loaded {
            self.refresh_graph();
        }
    }

    fn on_key(&mut self, key: KeyEvent) -> bool {
        if key.kind != KeyEventKind::Press {
            return false;
        }
        let shortcut = (key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT))
            || key.modifiers.contains(KeyModifiers::SUPER);

        if shortcut && matches!(key.code, KeyCode::Char('f' | 'F')) {
            self.set_view(View::Search);
            return false;
        }
        if shortcut && matches!(key.code, KeyCode::Char('1' | '2' | '3')) {
            self.set_view(view_for_key(key.code));
            return false;
        }
        if key.code == KeyCode::Esc {
            if self.view == View::Explorer {
                return true;
            }
            self.set_view(View::Explorer);
            return false;
        }

        if self.view == View::Search && self.search_focus {
            self.on_search_query_key(key, shortcut);
            return false;
        }

        if let KeyCode::Char('1' | '2' | '3') = key.code {
            self.set_view(view_for_key(key.code));
            return false;
        }
        if key.code == KeyCode::Char('q') {
            return true;
        }

        match self.view {
            View::Explorer => self.on_explorer_key(key),
            View::Search => self.on_search_results_key(key),
            View::GitGraph => self.on_graph_key(key),
        }
        false
    }

    fn on_search_query_key(&mut self, key: KeyEvent, shortcut: bool) {
        match key.code {
            KeyCode::Enter | KeyCode::Tab => self.search_focus = false,
            KeyCode::Backspace => {
                self.search_query.pop();
                self.schedule_search();
            }
            KeyCode::Up | KeyCode::Down => {
                self.search_focus = false;
                self.move_search_selection(if key.code == KeyCode::Up { -1 } else { 1 });
            }
            KeyCode::Char('a') if shortcut => {
                self.search_query.clear();
                self.schedule_search();
            }
            KeyCode::Char(ch) if !shortcut => {
                self.search_query.push(ch);
                self.schedule_search();
            }
            _ => {}
        }
    }

    fn on_search_results_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Tab | KeyCode::Char('/') => self.search_focus = true,
            KeyCode::Up | KeyCode::Char('k') => self.move_search_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_search_selection(1),
            KeyCode::Home | KeyCode::Char('g') => self.search_selected = 0,
            KeyCode::End | KeyCode::Char('G') => {
                self.search_selected = self.search_hits.len().saturating_sub(1);
            }
            KeyCode::Enter => {
                if let Some(hit) = self.search_hits.get(self.search_selected) {
                    self.status = Some(format!("{}:{}", hit.path, hit.line));
                }
            }
            KeyCode::Char('r') => self.schedule_search(),
            _ => {}
        }
    }

    fn on_graph_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.move_graph_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_graph_selection(1),
            KeyCode::Home | KeyCode::Char('g') => self.graph_selected = 0,
            KeyCode::End | KeyCode::Char('G') => {
                self.graph_selected = self.graph_lines.len().saturating_sub(1);
            }
            KeyCode::Enter => {
                if let Some(line) = self.graph_lines.get(self.graph_selected) {
                    self.status = Some(line.clone());
                }
            }
            KeyCode::Char('r') => self.refresh_graph(),
            _ => {}
        }
    }

    fn on_explorer_key(&mut self, key: KeyEvent) {
        let rows = self.tree.rows();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.explorer_selected = self.explorer_selected.saturating_sub(1);
                self.status = None;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.explorer_selected = self
                    .explorer_selected
                    .saturating_add(1)
                    .min(rows.len().saturating_sub(1));
                self.status = None;
            }
            KeyCode::Home => self.explorer_selected = 0,
            KeyCode::End => self.explorer_selected = rows.len().saturating_sub(1),
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                self.toggle_selected(&rows);
            }
            KeyCode::Left | KeyCode::Char('h') => {
                if let Some(row) = rows.get(self.explorer_selected) {
                    if row.is_dir && row.expanded {
                        self.tree.toggle(&row.path);
                    } else if row.depth > 0
                        && let Some(parent) =
                            rows[..self.explorer_selected]
                                .iter()
                                .rposition(|candidate| {
                                    candidate.is_dir
                                        && candidate.path
                                            == row.path.parent().unwrap_or(Path::new(""))
                                })
                    {
                        self.explorer_selected = parent;
                    }
                }
            }
            KeyCode::Char('.') => {
                self.tree.show_hidden = !self.tree.show_hidden;
                self.tree.refresh();
                self.explorer_selected = self
                    .explorer_selected
                    .min(self.tree.rows().len().saturating_sub(1));
            }
            KeyCode::Char('r') => {
                self.tree.refresh();
                self.status = Some("refreshed".into());
            }
            KeyCode::Char('c') => {
                self.tree.collapse_all();
                self.explorer_selected = self
                    .explorer_selected
                    .min(self.tree.rows().len().saturating_sub(1));
            }
            KeyCode::Char('i') => self.icon_theme = self.icon_theme.toggled(),
            KeyCode::Char(' ') => self.toggle_selected(&rows),
            _ => {}
        }
    }

    fn toggle_selected(&mut self, rows: &[Row]) {
        if let Some(row) = rows.get(self.explorer_selected) {
            if row.is_dir {
                self.tree.toggle(&row.path);
                self.status = None;
            } else {
                self.status = Some("File viewing is not part of this first pass".into());
            }
        }
    }

    fn on_mouse(&mut self, mouse: MouseEvent) {
        let position = Position::new(mouse.column, mouse.row);
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            for (index, button) in self.activity_buttons.iter().enumerate() {
                if button.contains(position) {
                    self.set_view([View::Explorer, View::Search, View::GitGraph][index]);
                    return;
                }
            }
            if self.header_actions[0].contains(position) {
                match self.view {
                    View::Explorer => {
                        self.tree.refresh();
                        self.status = Some("refreshed".into());
                    }
                    View::Search => self.schedule_search(),
                    View::GitGraph => self.refresh_graph(),
                }
                return;
            }
            if self.header_actions[1].contains(position) && self.view == View::Explorer {
                self.tree.collapse_all();
                self.explorer_selected = self
                    .explorer_selected
                    .min(self.tree.rows().len().saturating_sub(1));
                return;
            }
            if self.view == View::Search && self.query_area.contains(position) {
                self.search_focus = true;
                return;
            }
            if !self.body.contains(position) {
                return;
            }
            let index = self.body_index(mouse.row);
            match self.view {
                View::Explorer => {
                    let rows = self.tree.rows();
                    let Some(row) = rows.get(index) else { return };
                    self.explorer_selected = index;
                    self.status = None;
                    let disclosure_end = self
                        .body
                        .x
                        .saturating_add((row.depth as u16).saturating_mul(2))
                        .saturating_add(2);
                    if row.is_dir && mouse.column < disclosure_end {
                        self.tree.toggle(&row.path);
                    }
                }
                View::Search => {
                    if index < self.search_hits.len() {
                        self.search_selected = index;
                        self.search_focus = false;
                    }
                }
                View::GitGraph => {
                    if index < self.graph_lines.len() {
                        self.graph_selected = index;
                    }
                }
            }
        } else if matches!(
            mouse.kind,
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
        ) && self.body.contains(position)
        {
            let delta = if mouse.kind == MouseEventKind::ScrollUp {
                -3
            } else {
                3
            };
            match self.view {
                View::Explorer => {
                    self.explorer_selected =
                        move_selection(self.explorer_selected, delta, self.tree.rows().len());
                }
                View::Search => {
                    self.search_focus = false;
                    self.search_selected =
                        move_selection(self.search_selected, delta, self.search_hits.len());
                }
                View::GitGraph => {
                    self.graph_selected =
                        move_selection(self.graph_selected, delta, self.graph_lines.len());
                }
            }
        }
    }

    fn body_index(&self, row: u16) -> usize {
        self.body_scroll()
            .saturating_add(usize::from(row.saturating_sub(self.body.y)))
    }

    fn body_scroll(&self) -> usize {
        match self.view {
            View::Explorer => self.explorer_scroll,
            View::Search => self.search_scroll,
            View::GitGraph => self.graph_scroll,
        }
    }

    fn move_search_selection(&mut self, delta: isize) {
        self.search_selected = move_selection(self.search_selected, delta, self.search_hits.len());
        self.search_focus = false;
    }

    fn move_graph_selection(&mut self, delta: isize) {
        self.graph_selected = move_selection(self.graph_selected, delta, self.graph_lines.len());
    }

    fn schedule_search(&mut self) {
        self.search_hits.clear();
        self.search_selected = 0;
        self.search_scroll = 0;
        self.search_truncated = false;
        self.status = None;
        if self.search_query.is_empty() {
            self.search_loading = false;
            self.search_due = None;
        } else {
            self.search_loading = true;
            self.search_due = Some(Instant::now() + SEARCH_DEBOUNCE);
        }
    }

    fn start_search(&mut self) {
        let query = self.search_query.clone();
        let root = self.tree.root_path();
        let (tx, rx) = mpsc::channel();
        self.search_rx = Some(rx);
        self.search_loading = true;
        thread::spawn(move || {
            let output = search_content(&root, &query);
            let _ = tx.send(SearchCompletion { query, output });
        });
    }

    fn refresh_graph(&mut self) {
        if self.graph_rx.is_some() {
            return;
        }
        let root = self.tree.root_path();
        let (tx, rx) = mpsc::channel();
        self.graph_rx = Some(rx);
        self.graph_loading = true;
        self.graph_loaded = true;
        self.graph_error = None;
        self.graph_lines.clear();
        self.graph_selected = 0;
        self.graph_scroll = 0;
        thread::spawn(move || {
            let _ = tx.send(git_graph(&root));
        });
    }

    fn poll_workers(&mut self) {
        self.collect_search();
        self.collect_graph();
        if self.view == View::Search
            && self.search_rx.is_none()
            && self.search_due.is_some_and(|due| Instant::now() >= due)
        {
            self.search_due = None;
            self.start_search();
        }
    }

    fn collect_search(&mut self) {
        let Some(rx) = &self.search_rx else { return };
        match rx.try_recv() {
            Ok(done) => {
                self.search_rx = None;
                if done.query == self.search_query {
                    self.search_hits = done.output.hits;
                    self.search_truncated = done.output.truncated;
                    self.search_selected = 0;
                    self.search_scroll = 0;
                    self.search_loading = false;
                }
            }
            Err(TryRecvError::Disconnected) => {
                self.search_rx = None;
                self.search_loading = false;
            }
            Err(TryRecvError::Empty) => {}
        }
    }

    fn collect_graph(&mut self) {
        let Some(rx) = &self.graph_rx else { return };
        match rx.try_recv() {
            Ok(result) => {
                self.graph_rx = None;
                self.graph_loading = false;
                match result {
                    Ok(lines) => self.graph_lines = lines,
                    Err(error) => self.graph_error = Some(error),
                }
            }
            Err(TryRecvError::Disconnected) => {
                self.graph_rx = None;
                self.graph_loading = false;
                self.graph_error = Some("Git graph worker stopped unexpectedly".into());
            }
            Err(TryRecvError::Empty) => {}
        }
    }

    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        frame.render_widget(Block::default().style(Style::default().bg(BG)), area);
        let frame_block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(BORDER))
            .style(Style::default().bg(BG));
        let content = frame_block.inner(area);
        frame.render_widget(frame_block, area);
        let chunks = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(content);

        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    format!(" {} ", self.view.title()),
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    "  1 Explorer   2 Search   3 Git Graph",
                    Style::default().fg(MUTED),
                ),
            ]))
            .style(Style::default().bg(BG)),
            chunks[0],
        );
        self.draw_activity(frame, chunks[1]);
        self.draw_view_header(frame, chunks[2]);
        self.body = chunks[3];
        match self.view {
            View::Explorer => self.draw_explorer(frame, chunks[3]),
            View::Search => self.draw_search(frame, chunks[3]),
            View::GitGraph => self.draw_graph(frame, chunks[3]),
        }
        frame.render_widget(
            Paragraph::new(self.footer()).style(Style::default().fg(MUTED).bg(BG)),
            chunks[4],
        );
    }

    fn draw_activity(&mut self, frame: &mut Frame, area: Rect) {
        let widths = [
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Length(12),
        ];
        let slots = Layout::horizontal(widths).split(area);
        let labels = ["▣ Explorer", "⌕ Search", "⑂ Git Graph"];
        for index in 0..3 {
            self.activity_buttons[index] = slots[index];
            let selected = self.view.index() == index;
            let style = if selected {
                Style::default()
                    .fg(ACCENT)
                    .bg(SELECTED)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(MUTED).bg(BG)
            };
            frame.render_widget(Paragraph::new(labels[index]).style(style), slots[index]);
        }
    }

    fn draw_view_header(&mut self, frame: &mut Frame, area: Rect) {
        self.header_actions = [Rect::default(); 2];
        match self.view {
            View::Explorer => {
                let [label, refresh, collapse] = Layout::horizontal([
                    Constraint::Min(0),
                    Constraint::Length(4),
                    Constraint::Length(4),
                ])
                .areas(area);
                let root_name = terminal_safe(&self.tree.root_name());
                let root_icon = icons::icon(self.icon_theme, &root_name, true, true);
                frame.render_widget(
                    Paragraph::new(Line::from(vec![
                        Span::styled("▾ ", Style::default().fg(MUTED)),
                        icon_span(root_icon),
                        Span::styled(
                            format!(" {root_name}"),
                            Style::default().fg(FG).add_modifier(Modifier::BOLD),
                        ),
                    ]))
                    .style(Style::default().bg(BG)),
                    label,
                );
                self.header_actions = [refresh, collapse];
                frame.render_widget(
                    Paragraph::new(" ↻ ").style(Style::default().fg(MUTED).bg(BG)),
                    refresh,
                );
                frame.render_widget(
                    Paragraph::new(" ⌄ ").style(Style::default().fg(MUTED).bg(BG)),
                    collapse,
                );
            }
            View::Search => {
                self.query_area = area;
                let focus = if self.search_focus { "▏" } else { "" };
                let suffix = if self.search_truncated {
                    format!("  {}+ matches", SEARCH_MAX_HITS)
                } else if self.search_loading {
                    "  searching…".to_string()
                } else {
                    format!("  {} matches", self.search_hits.len())
                };
                frame.render_widget(
                    Paragraph::new(Line::from(vec![
                        Span::styled("⌕ ", Style::default().fg(ACCENT)),
                        Span::styled(terminal_safe(&self.search_query), Style::default().fg(FG)),
                        Span::styled(focus, Style::default().fg(ACCENT)),
                        Span::styled(suffix, Style::default().fg(MUTED)),
                    ]))
                    .style(Style::default().bg(BG)),
                    area,
                );
            }
            View::GitGraph => {
                let [label, refresh] =
                    Layout::horizontal([Constraint::Min(0), Constraint::Length(5)]).areas(area);
                frame.render_widget(
                    Paragraph::new(format!(
                        "Commits · {} (all refs)",
                        terminal_safe(&self.tree.root_name())
                    ))
                    .style(Style::default().fg(FG).bg(BG)),
                    label,
                );
                self.header_actions[0] = refresh;
                frame.render_widget(
                    Paragraph::new(" ↻ ").style(Style::default().fg(MUTED).bg(BG)),
                    refresh,
                );
            }
        }
    }

    fn draw_explorer(&mut self, frame: &mut Frame, body: Rect) {
        let rows = self.tree.rows();
        self.explorer_selected = self.explorer_selected.min(rows.len().saturating_sub(1));
        let height = body.height as usize;
        keep_visible(
            self.explorer_selected,
            &mut self.explorer_scroll,
            height,
            rows.len(),
        );
        let items = rows
            .iter()
            .enumerate()
            .skip(self.explorer_scroll)
            .take(height)
            .map(|(index, row)| row_item(row, self.icon_theme, index == self.explorer_selected))
            .collect::<Vec<_>>();
        frame.render_widget(List::new(items).style(Style::default().bg(BG)), body);
    }

    fn draw_search(&mut self, frame: &mut Frame, body: Rect) {
        self.search_selected = self
            .search_selected
            .min(self.search_hits.len().saturating_sub(1));
        let height = body.height as usize;
        keep_visible(
            self.search_selected,
            &mut self.search_scroll,
            height,
            self.search_hits.len(),
        );
        if self.search_query.is_empty() {
            frame.render_widget(
                Paragraph::new("Type to search text in this folder.")
                    .style(Style::default().fg(MUTED).bg(BG)),
                body,
            );
            return;
        }
        if self.search_hits.is_empty() {
            let message = if self.search_loading {
                "Searching…"
            } else {
                "No matching lines."
            };
            frame.render_widget(
                Paragraph::new(message).style(Style::default().fg(MUTED).bg(BG)),
                body,
            );
            return;
        }
        let items = self
            .search_hits
            .iter()
            .enumerate()
            .skip(self.search_scroll)
            .take(height)
            .map(|(index, hit)| search_item(hit, index == self.search_selected))
            .collect::<Vec<_>>();
        frame.render_widget(List::new(items).style(Style::default().bg(BG)), body);
    }

    fn draw_graph(&mut self, frame: &mut Frame, body: Rect) {
        self.graph_selected = self
            .graph_selected
            .min(self.graph_lines.len().saturating_sub(1));
        let height = body.height as usize;
        keep_visible(
            self.graph_selected,
            &mut self.graph_scroll,
            height,
            self.graph_lines.len(),
        );
        if let Some(error) = &self.graph_error {
            frame.render_widget(
                Paragraph::new(terminal_safe(error)).style(Style::default().fg(MUTED).bg(BG)),
                body,
            );
            return;
        }
        if self.graph_lines.is_empty() {
            let message = if self.graph_loading {
                "Loading Git history…"
            } else {
                "No commits found."
            };
            frame.render_widget(
                Paragraph::new(message).style(Style::default().fg(MUTED).bg(BG)),
                body,
            );
            return;
        }
        let items = self
            .graph_lines
            .iter()
            .enumerate()
            .skip(self.graph_scroll)
            .take(height)
            .map(|(index, line)| {
                let mut span = Span::styled(line.clone(), Style::default().fg(FG));
                if index == self.graph_selected {
                    span = span.clone().style(span.style.bg(SELECTED));
                }
                ListItem::new(Line::from(span))
            })
            .collect::<Vec<_>>();
        frame.render_widget(List::new(items).style(Style::default().bg(BG)), body);
    }

    fn footer(&self) -> String {
        if let Some(status) = &self.status {
            return terminal_safe(status);
        }
        match self.view {
            View::Explorer => {
                "↑↓ move  click select/arrow  Enter expand  . hidden  r refresh  c collapse  q quit"
                    .into()
            }
            View::Search => {
                if self.search_truncated {
                    format!(
                        "↑↓ select  Tab results/query  results capped at {SEARCH_MAX_HITS}  Esc back"
                    )
                } else {
                    "type search  Tab/Enter results  ↑↓ select  r refresh  Ctrl+F focus  Esc back"
                        .into()
                }
            }
            View::GitGraph => {
                "↑↓ select  Enter show line  r refresh  1 Explorer  2 Search  q quit".into()
            }
        }
    }
}

fn view_for_key(code: KeyCode) -> View {
    match code {
        KeyCode::Char('2') => View::Search,
        KeyCode::Char('3') => View::GitGraph,
        _ => View::Explorer,
    }
}

fn move_selection(selected: usize, delta: isize, len: usize) -> usize {
    if delta < 0 {
        selected.saturating_sub(delta.unsigned_abs())
    } else {
        selected
            .saturating_add(delta as usize)
            .min(len.saturating_sub(1))
    }
}

fn keep_visible(selected: usize, scroll: &mut usize, height: usize, len: usize) {
    *scroll = (*scroll).min(len.saturating_sub(height));
    if height == 0 {
        return;
    }
    if selected < *scroll {
        *scroll = selected;
    } else if selected >= *scroll + height {
        *scroll = selected + 1 - height;
    }
}

fn row_item(row: &Row, theme: icons::IconTheme, selected: bool) -> ListItem<'static> {
    let disclosure = if row.is_dir {
        if row.expanded { "▾ " } else { "▸ " }
    } else {
        "  "
    };
    let icon = icons::icon(theme, &row.name, row.is_dir, row.expanded);
    let mut spans = vec![
        Span::raw("  ".repeat(row.depth)),
        Span::styled(disclosure, Style::default().fg(MUTED)),
        icon_span(icon),
        Span::raw(" "),
        Span::styled(
            terminal_safe(&row.name),
            if row.is_dir {
                Style::default().fg(Color::Rgb(0xc5, 0xd7, 0xf5))
            } else {
                Style::default().fg(FG)
            },
        ),
    ];
    if selected {
        for span in &mut spans {
            *span = span.clone().style(span.style.bg(SELECTED));
        }
    }
    ListItem::new(Line::from(spans))
}

fn search_item(hit: &SearchHit, selected: bool) -> ListItem<'static> {
    let mut spans = vec![
        Span::styled(
            format!("{}:{} ", hit.path, hit.line),
            Style::default().fg(ACCENT),
        ),
        Span::styled(hit.context.clone(), Style::default().fg(FG)),
    ];
    if selected {
        for span in &mut spans {
            *span = span.clone().style(span.style.bg(SELECTED));
        }
    }
    ListItem::new(Line::from(spans))
}

fn icon_span(icon: icons::Icon) -> Span<'static> {
    let style = icon.rgb.map_or_else(Style::default, |(r, g, b)| {
        Style::default().fg(Color::Rgb(r, g, b))
    });
    Span::styled(icon.glyph, style)
}

fn search_content(root: &Path, query: &str) -> SearchOutput {
    if query.is_empty() {
        return SearchOutput::default();
    }
    let needle = query.to_lowercase();
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(false)
        .follow_links(false)
        .require_git(false)
        .max_filesize(Some(SEARCH_MAX_BYTES))
        .filter_entry(|entry| entry.file_name() != ".git");

    let mut output = SearchOutput::default();
    let mut visited = 0;
    'files: for entry in builder.build().flatten() {
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        visited += 1;
        if visited > SEARCH_MAX_FILES {
            output.truncated = true;
            break;
        }
        let path = entry.into_path();
        let mut bytes = Vec::new();
        let Ok(file) = File::open(&path) else {
            continue;
        };
        if file
            .take(SEARCH_MAX_BYTES + 1)
            .read_to_end(&mut bytes)
            .is_err()
            || bytes.len() as u64 > SEARCH_MAX_BYTES
            || bytes.contains(&0)
        {
            continue;
        }
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };
        let label = terminal_safe(
            &path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/"),
        );
        for (index, line) in text.lines().enumerate() {
            if !line.to_lowercase().contains(&needle) {
                continue;
            }
            output.hits.push(SearchHit {
                path: label.clone(),
                line: index + 1,
                context: truncate_chars(&terminal_safe(line), 160),
            });
            if output.hits.len() >= SEARCH_MAX_HITS {
                output.truncated = true;
                break 'files;
            }
        }
    }
    output
}

fn git_graph(root: &Path) -> Result<Vec<String>, String> {
    let output = Command::new("git")
        .args([
            "--no-pager",
            "-c",
            "color.ui=false",
            "log",
            "--graph",
            "--oneline",
            "--decorate=short",
            "--all",
            "--max-count=200",
            "--no-color",
        ])
        .current_dir(root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .map_err(|error| format!("could not run git: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let message = stderr
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("git log failed");
        return Err(terminal_safe(message));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .take(GRAPH_MAX_COMMITS * 3)
        .map(terminal_safe)
        .collect())
}

fn truncate_chars(text: &str, max: usize) -> String {
    let mut chars = text.chars();
    let prefix = chars.by_ref().take(max).collect::<String>();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

fn terminal_safe(text: &str) -> String {
    text.chars()
        .map(|ch| if ch.is_control() { '�' } else { ch })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("teditor-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn git(root: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .env("GIT_AUTHOR_NAME", "teditor test")
            .env("GIT_AUTHOR_EMAIL", "teditor@example.invalid")
            .env("GIT_COMMITTER_NAME", "teditor test")
            .env("GIT_COMMITTER_EMAIL", "teditor@example.invalid")
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    #[test]
    fn mouse_click_selects_rows_and_disclosure_expands_folders() {
        let root = temp_dir("mouse");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("z.txt"), "").unwrap();
        let mut app = App::new(root.clone(), None);
        app.body = Rect::new(2, 3, 40, 10);

        handle_event(
            &mut app,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 8,
                row: 4,
                modifiers: KeyModifiers::NONE,
            }),
        );
        assert_eq!(
            app.explorer_selected, 1,
            "click selects the row under the pointer"
        );

        handle_event(
            &mut app,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 3,
                row: 3,
                modifiers: KeyModifiers::NONE,
            }),
        );
        assert!(
            app.tree.rows()[0].expanded,
            "clicking the chevron expands it"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn folder_enter_expands_and_collapses() {
        let root = temp_dir("keyboard");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.rs"), "").unwrap();
        let mut app = App::new(root.clone(), None);
        app.on_explorer_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.tree.rows()[0].expanded);
        app.on_explorer_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!app.tree.rows()[0].expanded);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn keyboard_and_activity_click_switch_views() {
        let root = temp_dir("views");
        let mut app = App::new(root.clone(), None);
        app.on_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
        assert!(app.view == View::Search);
        assert!(app.search_focus);

        app.graph_loaded = true;
        app.on_key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::CONTROL));
        assert!(app.view == View::GitGraph);

        app.activity_buttons[0] = Rect::new(1, 1, 12, 1);
        handle_event(
            &mut app,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 2,
                row: 1,
                modifiers: KeyModifiers::NONE,
            }),
        );
        assert!(app.view == View::Explorer);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn content_search_returns_relative_path_and_line_and_honors_gitignore() {
        let root = temp_dir("search");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(root.join("src/readme.md"), "first\nNeedle here\n").unwrap();
        std::fs::write(root.join("ignored.txt"), "needle hidden\n").unwrap();

        let result = search_content(&root, "needle");
        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].path, "src/readme.md");
        assert_eq!(result.hits[0].line, 2);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn git_graph_view_renders_title_and_commit_rows() {
        let root = temp_dir("graph-render");
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.name", "teditor test"]);
        git(&root, &["config", "user.email", "teditor@example.invalid"]);
        std::fs::write(root.join("file.txt"), "content\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-q", "-m", "rendered graph commit"]);
        let mut app = App::new(root.clone(), None);
        app.view = View::GitGraph;
        app.graph_loaded = true;
        app.graph_lines = git_graph(&root).unwrap();
        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let screen = terminal.backend().to_string();
        assert!(screen.contains("GIT GRAPH"));
        assert!(screen.contains("rendered graph commit"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn git_graph_shows_branch_merge_edges_and_refs() {
        let root = temp_dir("graph");
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.name", "teditor test"]);
        git(&root, &["config", "user.email", "teditor@example.invalid"]);
        std::fs::write(root.join("root.txt"), "root\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-q", "-m", "base"]);
        git(&root, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(root.join("feature.txt"), "feature\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-q", "-m", "feature work"]);
        git(&root, &["checkout", "-q", "main"]);
        std::fs::write(root.join("main.txt"), "main\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-q", "-m", "main work"]);
        git(&root, &["merge", "-q", "--no-ff", "--no-edit", "feature"]);

        let graph = git_graph(&root).unwrap();
        assert!(graph.iter().any(|line| line.contains("feature")));
        assert!(
            graph
                .iter()
                .any(|line| line.contains("|\\") || line.contains("|/"))
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
