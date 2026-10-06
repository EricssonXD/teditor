mod editor;
mod icons;
mod scm;
mod scm_view;
mod search;
mod search_view;
mod tree;

use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use editor::{EditAction, Editor, SaveOutcome};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use scm_view::{DrawerRef, ScmAction, ScmView};
use search_view::{SearchAction, SearchView};
use tree::{Row, Tree};
use unicode_width::UnicodeWidthChar;

pub(crate) const BG: Color = Color::Rgb(0x1b, 0x1d, 0x29);
pub(crate) const FG: Color = Color::Rgb(0xd2, 0xd6, 0xe0);
pub(crate) const MUTED: Color = Color::Rgb(0x78, 0x82, 0x9b);
pub(crate) const ACCENT: Color = Color::Rgb(0x82, 0xaa, 0xff);
pub(crate) const BORDER: Color = Color::Rgb(0x3d, 0x4a, 0x70);
pub(crate) const SELECTED: Color = Color::Rgb(0x36, 0x42, 0x62);
const MAX_PREVIEW_BYTES: usize = 1024 * 1024;

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
        app.tick();
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
    SourceControl,
}

impl View {
    fn index(self) -> usize {
        match self {
            Self::Explorer => 0,
            Self::Search => 1,
            Self::SourceControl => 2,
        }
    }
}

#[derive(Clone)]
struct EditTarget {
    path: PathBuf,
    root: PathBuf,
    relative: PathBuf,
    staged: bool,
}

struct Preview {
    title: String,
    lines: Vec<String>,
    line_numbers: bool,
    anchor: usize,
    scroll: usize,
    edit_target: Option<EditTarget>,
}

impl Preview {
    fn new(title: String, text: String, line_numbers: bool, anchor: usize) -> Self {
        let mut lines = text.lines().map(terminal_safe).collect::<Vec<_>>();
        if lines.is_empty() {
            lines.push("(empty)".into());
        }
        let anchor = anchor.min(lines.len().saturating_sub(1));
        Self {
            title,
            lines,
            line_numbers,
            anchor,
            scroll: 0,
            edit_target: None,
        }
    }
}

struct App {
    tree: Tree,
    view: View,
    icon_theme: icons::IconTheme,
    search: SearchView,
    scm: ScmView,
    status: Option<String>,
    explorer_selected: usize,
    explorer_scroll: usize,
    body: Rect,
    activity_buttons: [Rect; 3],
    header_actions: [Rect; 2],
    mouse_pos: Option<(u16, u16)>,
    preview: Option<Preview>,
    editor: Option<Editor>,
    editor_body: Rect,
}

impl App {
    fn new(root: PathBuf, selected_path: Option<PathBuf>) -> Self {
        let mut tree = Tree::new(root.clone());
        let rows = tree.rows();
        let explorer_selected = selected_path
            .and_then(|path| rows.iter().position(|row| row.path == path))
            .unwrap_or(0);
        Self {
            tree,
            view: View::Explorer,
            icon_theme: icons::IconTheme::resolve(None, None),
            search: SearchView::new(),
            scm: ScmView::new(&root),
            status: None,
            explorer_selected,
            explorer_scroll: 0,
            body: Rect::default(),
            activity_buttons: [Rect::default(); 3],
            header_actions: [Rect::default(); 2],
            mouse_pos: None,
            preview: None,
            editor: None,
            editor_body: Rect::default(),
        }
    }

    fn tick(&mut self) {
        self.search
            .tick(&self.tree.root_path(), self.tree.show_hidden);
    }

    fn switch_view(&mut self, view: View, focus_query: bool) {
        if self.search.is_active() && view != View::Search {
            self.search.close();
        }
        if view == View::Search {
            self.search.open(focus_query);
        }
        if view == View::SourceControl && self.view != View::SourceControl {
            self.scm.refresh();
        }
        self.view = view;
        self.status = None;
    }

    fn on_key(&mut self, key: KeyEvent) -> bool {
        if key.kind != KeyEventKind::Press {
            return false;
        }
        if self.editor.is_some() {
            return self.on_editor_key(key);
        }
        if self.preview.is_some() {
            return self.on_preview_key(key);
        }
        let shortcut = (key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT))
            || key.modifiers.contains(KeyModifiers::SUPER);

        if shortcut && matches!(key.code, KeyCode::Char('f' | 'F')) {
            self.switch_view(View::Search, true);
            return false;
        }
        if shortcut && matches!(key.code, KeyCode::Char('1' | '2' | '3')) {
            self.switch_view(view_for_key(key.code), false);
            return false;
        }

        match self.view {
            View::Explorer => {
                if let KeyCode::Char('1' | '2' | '3') = key.code {
                    self.switch_view(view_for_key(key.code), false);
                    return false;
                }
                self.on_explorer_key(key)
            }
            View::Search => {
                if self.search.text_unfocused()
                    && let KeyCode::Char('1' | '2' | '3') = key.code
                    && key.modifiers.is_empty()
                {
                    self.switch_view(view_for_key(key.code), false);
                    return false;
                }
                let action = self.search.on_key(key);
                self.on_search_action(action)
            }
            View::SourceControl => {
                let action = self.scm.on_key(key);
                self.on_scm_action(action)
            }
        }
    }

    fn on_search_action(&mut self, action: SearchAction) -> bool {
        match action {
            SearchAction::None => false,
            SearchAction::Close => {
                self.switch_view(View::Explorer, false);
                false
            }
            SearchAction::OpenHit(path, line) => {
                let root = self.tree.root_path();
                self.open_editor(&root.join(path), line.saturating_sub(1));
                false
            }
        }
    }

    fn on_scm_action(&mut self, action: ScmAction) -> bool {
        match action {
            ScmAction::None => false,
            ScmAction::Quit => true,
            ScmAction::SwitchToExplorer => {
                self.switch_view(View::Explorer, false);
                false
            }
            ScmAction::SwitchToSearch => {
                self.switch_view(View::Search, false);
                false
            }
            ScmAction::OpenCommit(commit) => {
                if let Some(root) = self.scm.active_root().map(Path::to_path_buf) {
                    self.open_reference_preview(&root, DrawerRef::Commit(commit));
                }
                false
            }
            ScmAction::OpenFile { root, path, staged } => {
                self.open_file_preview(&root, Path::new(&path), Some(staged), 0);
                false
            }
            ScmAction::OpenReference { root, reference } => {
                self.open_reference_preview(&root, reference);
                false
            }
        }
    }

    fn open_file_preview(
        &mut self,
        root: &Path,
        relative: &Path,
        staged: Option<bool>,
        anchor: usize,
    ) {
        let path = if relative.is_absolute() {
            relative.to_path_buf()
        } else {
            root.join(relative)
        };
        self.status = None;
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("file");
        if let Some(staged) = staged {
            match git_diff(root, relative, staged) {
                Ok(diff) if !diff.trim().is_empty() => {
                    let preview = Preview::new(
                        format!(
                            "{} · {name}",
                            if staged { "Staged Changes" } else { "Changes" }
                        ),
                        diff,
                        false,
                        0,
                    );
                    self.preview = Some(preview);
                    return;
                }
                Err(error) => {
                    let preview = Preview::new(format!("Changes · {name}"), error, false, 0);
                    self.preview = Some(preview);
                    return;
                }
                _ => {}
            }
        }
        let edit_target = staged.map(|staged| EditTarget {
            path: path.clone(),
            root: root.to_path_buf(),
            relative: relative.to_path_buf(),
            staged,
        });
        let (text, error) = read_preview_file(&path);
        let mut preview = Preview::new(
            format!(
                "{}{}",
                if error { "Preview error · " } else { "" },
                path.display()
            ),
            text,
            true,
            anchor,
        );
        preview.edit_target = edit_target;
        self.preview = Some(preview);
    }

    fn open_reference_preview(&mut self, root: &Path, reference: DrawerRef) {
        self.status = None;
        let (title, text) = match reference {
            DrawerRef::None => return,
            DrawerRef::Remote { name, url } => {
                (format!("Remote · {name}"), format!("{name}\n{url}"))
            }
            DrawerRef::Worktree(path) => (
                "Worktree".into(),
                format!(
                    "{path}\n\nGit worktrees keep independent working trees for one repository."
                ),
            ),
            DrawerRef::Commit(hash) => git_reference_preview(root, &hash),
            DrawerRef::Stash(index) => git_reference_preview(root, &format!("stash@{{{index}}}")),
            DrawerRef::Branch { name, .. } | DrawerRef::Tag(name) => {
                git_reference_preview(root, &name)
            }
        };
        self.preview = Some(Preview::new(title, text, false, 0));
    }

    fn open_editor(&mut self, path: &Path, line: usize) {
        match Editor::open(path, MAX_PREVIEW_BYTES) {
            Ok(mut editor) => {
                editor.place_cursor(line, 0);
                self.editor = Some(editor);
                self.status = None;
            }
            Err(error) => {
                self.status = Some(format!("Cannot edit {}: {error}", path.display()));
            }
        }
    }

    fn on_editor_key(&mut self, key: KeyEvent) -> bool {
        let page = usize::from(self.editor_body.height).max(1);
        let action = self
            .editor
            .as_mut()
            .map_or(EditAction::None, |editor| editor.handle_key(key, page));
        match action {
            EditAction::None => {
                if self.editor.as_ref().is_some_and(Editor::dirty) {
                    self.status = None;
                }
            }
            EditAction::Close => {
                if self.editor.as_ref().is_some_and(Editor::dirty) {
                    self.status = Some("Unsaved changes · Ctrl+S save / Ctrl+Q discard".into());
                } else {
                    self.editor = None;
                    self.status = None;
                }
            }
            EditAction::Discard => {
                self.editor = None;
                self.status = None;
            }
            EditAction::Save => {
                let force = self.editor.as_ref().is_some_and(Editor::external_changed);
                match self.editor.as_mut().expect("editor exists").save(force) {
                    Ok(SaveOutcome::Saved) => {
                        self.refresh_edit_preview();
                        self.status = Some("Saved".into());
                    }
                    Ok(SaveOutcome::Conflict) => {
                        self.status =
                            Some("File changed on disk · Ctrl+S again to overwrite".into());
                    }
                    Err(error) => self.status = Some(error),
                }
            }
            EditAction::Reload => {
                if self.editor.as_ref().is_some_and(Editor::dirty) {
                    self.status = Some("Unsaved changes · save or Ctrl+Q before reload".into());
                } else {
                    match self
                        .editor
                        .as_mut()
                        .expect("editor exists")
                        .reload(MAX_PREVIEW_BYTES)
                    {
                        Ok(()) => self.status = Some("Reloaded".into()),
                        Err(error) => self.status = Some(format!("Unable to reload: {error}")),
                    }
                }
            }
        }
        false
    }

    fn refresh_edit_preview(&mut self) {
        let target = self
            .preview
            .as_ref()
            .and_then(|preview| preview.edit_target.clone());
        if let Some(target) = target
            && self
                .editor
                .as_ref()
                .is_some_and(|editor| editor.path() == target.path)
        {
            self.open_file_preview(&target.root, &target.relative, Some(target.staged), 0);
        }
    }

    fn on_preview_key(&mut self, key: KeyEvent) -> bool {
        if key.code == KeyCode::Char('e')
            && let Some(path) = self
                .preview
                .as_ref()
                .and_then(|preview| preview.edit_target.as_ref())
                .map(|target| target.path.clone())
        {
            self.open_editor(&path, 0);
            return false;
        }
        if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
            self.preview = None;
            return false;
        }
        let Some(preview) = &mut self.preview else {
            return false;
        };
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                if preview.line_numbers {
                    preview.anchor = preview.anchor.saturating_sub(1);
                } else {
                    preview.scroll = preview.scroll.saturating_sub(1);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if preview.line_numbers {
                    preview.anchor =
                        (preview.anchor + 1).min(preview.lines.len().saturating_sub(1));
                } else {
                    preview.scroll =
                        (preview.scroll + 1).min(preview.lines.len().saturating_sub(1));
                }
            }
            KeyCode::PageUp => {
                if preview.line_numbers {
                    preview.anchor = preview.anchor.saturating_sub(10);
                } else {
                    preview.scroll = preview.scroll.saturating_sub(10);
                }
            }
            KeyCode::PageDown => {
                if preview.line_numbers {
                    preview.anchor =
                        (preview.anchor + 10).min(preview.lines.len().saturating_sub(1));
                } else {
                    preview.scroll =
                        (preview.scroll + 10).min(preview.lines.len().saturating_sub(1));
                }
            }
            KeyCode::Home | KeyCode::Char('g') => {
                preview.anchor = 0;
                preview.scroll = 0;
            }
            KeyCode::End | KeyCode::Char('G') => {
                preview.anchor = preview.lines.len().saturating_sub(1);
                preview.scroll = preview.anchor;
            }
            _ => {}
        }
        false
    }

    fn on_explorer_key(&mut self, key: KeyEvent) -> bool {
        let rows = self.tree.rows();
        match key.code {
            KeyCode::Char('q') => return true,
            KeyCode::Esc => {}
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
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') | KeyCode::Char(' ') => {
                self.toggle_selected(&rows);
            }
            KeyCode::Char('e') => self.toggle_selected(&rows),
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
            _ => {}
        }
        false
    }

    fn toggle_selected(&mut self, rows: &[Row]) {
        if let Some(row) = rows.get(self.explorer_selected) {
            if row.is_dir {
                self.tree.toggle(&row.path);
                self.status = None;
            } else {
                self.open_editor(&row.path, 0);
            }
        }
    }

    fn on_mouse(&mut self, mouse: MouseEvent) {
        self.mouse_pos = Some((mouse.column, mouse.row));
        if let Some(editor) = &mut self.editor {
            editor.on_mouse(mouse, self.editor_body);
            self.status = None;
            return;
        }
        if let Some(preview) = &mut self.preview {
            if mouse.kind == MouseEventKind::ScrollUp {
                if preview.line_numbers {
                    preview.anchor = preview.anchor.saturating_sub(3);
                } else {
                    preview.scroll = preview.scroll.saturating_sub(3);
                }
            } else if mouse.kind == MouseEventKind::ScrollDown {
                if preview.line_numbers {
                    preview.anchor =
                        (preview.anchor + 3).min(preview.lines.len().saturating_sub(1));
                } else {
                    preview.scroll =
                        (preview.scroll + 3).min(preview.lines.len().saturating_sub(1));
                }
            }
            return;
        }
        if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
            for (index, button) in self.activity_buttons.iter().enumerate() {
                if button.contains(Position::new(mouse.column, mouse.row)) {
                    self.switch_view(
                        [View::Explorer, View::Search, View::SourceControl][index],
                        false,
                    );
                    return;
                }
            }
        }
        match self.view {
            View::Search => {
                let action = self.search.on_mouse(mouse);
                self.on_search_action(action);
            }
            View::SourceControl => {
                let action = self.scm.on_mouse(mouse);
                let _ = self.on_scm_action(action);
            }
            View::Explorer => self.on_explorer_mouse(mouse),
        }
    }

    fn on_explorer_mouse(&mut self, mouse: MouseEvent) {
        let position = Position::new(mouse.column, mouse.row);
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            if self.header_actions[0].contains(position) {
                self.tree.refresh();
                self.status = Some("refreshed".into());
                return;
            }
            if self.header_actions[1].contains(position) {
                self.tree.collapse_all();
                self.explorer_selected = self
                    .explorer_selected
                    .min(self.tree.rows().len().saturating_sub(1));
                return;
            }
            if !self.body.contains(position) {
                return;
            }
            let index = self.body_index(mouse.row);
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
            self.explorer_selected =
                move_selection(self.explorer_selected, delta, self.tree.rows().len());
        }
    }

    fn body_index(&self, row: u16) -> usize {
        self.explorer_scroll
            .saturating_add(usize::from(row.saturating_sub(self.body.y)))
    }

    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        frame.render_widget(Block::default().style(Style::default().bg(BG)), area);
        let border = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(BORDER))
            .style(Style::default().bg(BG));
        let content = border.inner(area);
        frame.render_widget(border, area);
        let [activity, remaining] =
            Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).areas(content);
        self.draw_activity(frame, activity);
        if self.preview.is_none() && self.editor.is_none() && self.view == View::SourceControl {
            self.scm
                .draw(frame, remaining, self.icon_theme, self.mouse_pos);
            return;
        }
        let [panel, footer] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(remaining);
        if self.editor.is_some() {
            self.draw_editor(frame, panel);
        } else if self.preview.is_some() {
            self.draw_preview(frame, panel);
        } else {
            match self.view {
                View::Explorer => {
                    let [header, body] =
                        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(panel);
                    self.draw_explorer_header(frame, header);
                    self.body = body;
                    self.draw_explorer(frame, body);
                }
                View::Search => self
                    .search
                    .draw(frame, panel, self.icon_theme, self.mouse_pos),
                View::SourceControl => unreachable!(),
            }
        }
        frame.render_widget(
            Paragraph::new(self.footer()).style(Style::default().fg(MUTED).bg(BG)),
            footer,
        );
    }

    fn draw_activity(&mut self, frame: &mut Frame, area: Rect) {
        let widths = [Constraint::Length(5); 3];
        let slots = Layout::horizontal(widths).split(area);
        let glyphs = match self.icon_theme {
            icons::IconTheme::Material => ["\u{f07b}", "\u{f002}", "\u{f126}"],
            icons::IconTheme::Emoji => ["📁", "🔍", "🔀"],
        };
        for (index, slot) in slots.iter().enumerate() {
            self.activity_buttons[index] = Rect::new(slot.x, area.y, slot.width, area.height);
            let active = self.view.index() == index;
            let style = if active {
                Style::default()
                    .fg(ACCENT)
                    .bg(SELECTED)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(MUTED).bg(BG)
            };
            frame.render_widget(Block::default().style(style), *slot);
            let middle = Rect::new(slot.x, slot.y + slot.height / 2, slot.width, 1);
            frame.render_widget(
                Paragraph::new(glyphs[index])
                    .alignment(ratatui::layout::Alignment::Center)
                    .style(style),
                middle,
            );
        }
    }

    fn draw_explorer_header(&mut self, frame: &mut Frame, area: Rect) {
        self.header_actions = [Rect::default(); 2];
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

    fn draw_explorer(&mut self, frame: &mut Frame, body: Rect) {
        let rows = self.tree.rows();
        self.explorer_selected = self.explorer_selected.min(rows.len().saturating_sub(1));
        let height = usize::from(body.height);
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

    fn draw_preview(&mut self, frame: &mut Frame, area: Rect) {
        let Some(preview) = &mut self.preview else {
            return;
        };
        let [header, body] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
        frame.render_widget(
            Paragraph::new(terminal_safe(&preview.title))
                .style(Style::default().fg(FG).bg(BG).add_modifier(Modifier::BOLD)),
            header,
        );
        keep_visible(
            preview.anchor,
            &mut preview.scroll,
            usize::from(body.height),
            preview.lines.len(),
        );
        let rows = preview
            .lines
            .iter()
            .enumerate()
            .skip(preview.scroll)
            .take(usize::from(body.height))
            .map(|(index, text)| {
                let mut spans = Vec::new();
                if preview.line_numbers {
                    spans.push(Span::styled(
                        format!("{:>5} ", index + 1),
                        Style::default().fg(MUTED),
                    ));
                }
                spans.push(Span::styled(terminal_safe(text), preview_line_style(text)));
                let row_bg = if preview.line_numbers && index == preview.anchor {
                    SELECTED
                } else {
                    BG
                };
                for span in &mut spans {
                    *span = span.clone().style(span.style.bg(row_bg));
                }
                ListItem::new(Line::from(spans))
            })
            .collect::<Vec<_>>();
        frame.render_widget(List::new(rows).style(Style::default().bg(BG)), body);
    }

    fn draw_editor(&mut self, frame: &mut Frame, area: Rect) {
        let Some(editor) = &mut self.editor else {
            return;
        };
        let [header, body] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
        editor.ensure_visible(usize::from(body.width), usize::from(body.height));
        self.editor_body = body;
        let marker = if editor.dirty() { " *" } else { "" };
        frame.render_widget(
            Paragraph::new(format!(" {}{marker}", editor.path().display()))
                .style(Style::default().fg(FG).bg(BG).add_modifier(Modifier::BOLD)),
            header,
        );
        let rows = editor
            .lines()
            .iter()
            .enumerate()
            .skip(editor.scroll())
            .take(usize::from(body.height))
            .map(|(index, line)| {
                let current = index == editor.cursor_line();
                let mut spans = vec![Span::styled(
                    format!("{:>5} ", index + 1),
                    Style::default().fg(if current { ACCENT } else { MUTED }),
                )];
                spans.extend(editor_line_spans(
                    line,
                    current.then_some(editor.cursor_col()),
                    editor.horizontal_scroll(),
                ));
                ListItem::new(Line::from(spans))
            })
            .collect::<Vec<_>>();
        frame.render_widget(List::new(rows).style(Style::default().bg(BG)), body);
    }

    fn footer(&self) -> String {
        if self.editor.is_some() {
            if let Some(status) = &self.status {
                return terminal_safe(status);
            }
            return "Type · Ctrl+S save · Esc back · Ctrl+Q discard · Ctrl+R reload".into();
        }
        if let Some(status) = &self.status {
            return terminal_safe(status);
        }
        if let Some(preview) = &self.preview {
            return if preview.edit_target.is_some() {
                "↑↓ scroll · e edit file · Esc back".into()
            } else {
                "↑↓ scroll  PgUp/PgDn page  Esc back".into()
            };
        }
        match self.view {
            View::Explorer => "↑↓ move  click select/arrow  Enter expand  . hidden  r refresh  c collapse  2 Search  3 Source Control  q quit".into(),
            View::Search => "type search  Tab fields/results  ↑↓ select  Enter open  Ctrl+F focus  Esc back".into(),
            View::SourceControl => "↑↓ move  Enter expand/stage  a stage all  u unstage all  r refresh  2 Search  q quit".into(),
        }
    }
}

fn editor_line_spans(
    line: &[char],
    cursor_col: Option<usize>,
    horizontal_scroll: usize,
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut cells = 0;
    for (index, ch) in line.iter().enumerate() {
        let width = if *ch == '\t' {
            4 - cells % 4
        } else {
            ch.width().unwrap_or(0)
        };
        if cells + width <= horizontal_scroll {
            cells += width;
            continue;
        }
        let skip = horizontal_scroll.saturating_sub(cells).min(width);
        let shown = if *ch == '\t' {
            " ".repeat(width - skip)
        } else {
            terminal_safe(&ch.to_string())
        };
        let style = if cursor_col == Some(index) {
            Style::default().fg(BG).bg(ACCENT)
        } else {
            Style::default().fg(FG)
        };
        spans.push(Span::styled(shown, style));
        cells += width;
    }
    if cursor_col == Some(line.len()) && cells >= horizontal_scroll {
        spans.push(Span::styled(" ", Style::default().fg(BG).bg(ACCENT)));
    }
    if spans.is_empty() {
        spans.push(Span::raw(""));
    }
    spans
}

fn preview_line_style(line: &str) -> Style {
    if line.starts_with("+++") || line.starts_with("---") {
        Style::default().fg(MUTED)
    } else if line.starts_with('+') {
        Style::default().fg(Color::Rgb(0x8b, 0xd4, 0x9c))
    } else if line.starts_with('-') {
        Style::default().fg(Color::Rgb(0xe4, 0x67, 0x6b))
    } else if line.starts_with("@@") {
        Style::default().fg(ACCENT)
    } else {
        Style::default().fg(FG)
    }
}

fn read_preview_file(path: &Path) -> (String, bool) {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) => return (format!("Unable to open file: {error}"), true),
    };
    let mut bytes = Vec::new();
    if let Err(error) = file
        .take((MAX_PREVIEW_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
    {
        return (format!("Unable to read file: {error}"), true);
    }
    if bytes.len() > MAX_PREVIEW_BYTES {
        return ("Preview limited to files up to 1 MiB".into(), true);
    }
    match String::from_utf8(bytes) {
        Ok(text) => (text, false),
        Err(_) => ("This file is not valid UTF-8 text".into(), true),
    }
}

// ponytail: one-file Git previews are capped at 1 MiB for display; stream Git output if large diffs become common.
fn git_diff(root: &Path, relative: &Path, staged: bool) -> Result<String, String> {
    let mut command = Command::new("git");
    command.current_dir(root).args([
        "-c",
        "color.ui=false",
        "diff",
        "--no-ext-diff",
        "--unified=3",
    ]);
    if staged {
        command.arg("--cached");
    }
    let output = command
        .arg("--")
        .arg(relative)
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    Ok(bounded_text(&output.stdout))
}

fn git_reference_preview(root: &Path, reference: &str) -> (String, String) {
    let title = format!("Git Reference · {reference}");
    let revision = format!("{reference}^{{object}}");
    let resolve = Command::new("git")
        .current_dir(root)
        .args(["rev-parse", "--verify", "--end-of-options"])
        .arg(revision)
        .output();
    let Ok(resolve) = resolve else {
        return (title, "Unable to run git rev-parse".into());
    };
    if !resolve.status.success() {
        return (
            title,
            String::from_utf8_lossy(&resolve.stderr).trim().to_owned(),
        );
    }
    let object = String::from_utf8_lossy(&resolve.stdout).trim().to_owned();
    if object.is_empty() || !object.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return (title, "Git returned an invalid object id".into());
    }
    let show = Command::new("git")
        .current_dir(root)
        .args([
            "-c",
            "color.ui=false",
            "show",
            "--format=fuller",
            "--stat",
            "--stat-count=20",
            "--no-patch",
            "--no-ext-diff",
        ])
        .arg(object)
        .output();
    match show {
        Ok(output) if output.status.success() => (title, bounded_text(&output.stdout)),
        Ok(output) => (
            title,
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ),
        Err(error) => (title, format!("Unable to run git show: {error}")),
    }
}

fn bounded_text(bytes: &[u8]) -> String {
    let length = bytes.len().min(MAX_PREVIEW_BYTES);
    let mut text = String::from_utf8_lossy(&bytes[..length]).into_owned();
    if bytes.len() > length {
        text.push_str("\n… output truncated at 1 MiB");
    }
    text
}

fn view_for_key(code: KeyCode) -> View {
    match code {
        KeyCode::Char('2') => View::Search,
        KeyCode::Char('3') => View::SourceControl,
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

fn icon_span(icon: icons::Icon) -> Span<'static> {
    let style = icon.rgb.map_or_else(Style::default, |(r, g, b)| {
        Style::default().fg(Color::Rgb(r, g, b))
    });
    Span::styled(icon.glyph, style)
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

    #[test]
    fn folder_enter_expands_and_collapses() {
        let root = temp_dir("keyboard");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.rs"), "").unwrap();
        let mut app = App::new(root.clone(), None);
        assert!(!app.on_explorer_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
        assert!(app.tree.rows()[0].expanded);
        app.on_explorer_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!app.tree.rows()[0].expanded);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn explorer_and_search_edit_files_inline_and_guard_unsaved_changes() {
        let root = temp_dir("inline-editor");
        std::fs::write(root.join("file.txt"), "before\n").unwrap();
        let mut app = App::new(root.clone(), None);
        let original_view = app.view;
        app.on_explorer_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.editor.is_some());
        assert!(app.preview.is_none());
        assert!(app.view == original_view);

        app.on_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        app.on_key(KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE));
        app.on_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        assert_eq!(
            std::fs::read_to_string(root.join("file.txt")).unwrap(),
            "before!\n"
        );
        assert!(app.view == original_view);

        app.on_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));
        assert!(!app.on_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE)));
        assert!(!app.on_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)));
        assert!(
            app.view == original_view,
            "view/quit keys must not escape a dirty editor"
        );
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.editor.is_some(), "Esc must not discard unsaved text");
        app.on_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL));
        assert!(app.editor.is_none());

        app.switch_view(View::Search, true);
        app.on_search_action(SearchAction::OpenHit("file.txt".into(), 1));
        assert!(app.editor.is_some());
        assert!(app.view == View::Search);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn scm_open_actions_show_local_diff_and_reference_previews() {
        let root = temp_dir("git-preview");
        let git = |args: &[&str]| {
            let status = Command::new("git")
                .current_dir(&root)
                .args(args)
                .env("GIT_AUTHOR_NAME", "Teditor")
                .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
                .env("GIT_COMMITTER_NAME", "Teditor")
                .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-q"]);
        std::fs::write(root.join("change.txt"), "before\n").unwrap();
        git(&["add", "change.txt"]);
        git(&["commit", "-qm", "preview commit"]);
        std::fs::write(root.join("change.txt"), "after\n").unwrap();

        let mut app = App::new(root.clone(), None);
        assert!(!app.on_scm_action(ScmAction::OpenFile {
            root: root.clone(),
            path: "change.txt".into(),
            staged: false,
        }));
        assert!(
            app.preview
                .as_ref()
                .unwrap()
                .lines
                .join("\n")
                .contains("+after")
        );
        app.on_preview_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
        assert!(app.editor.is_none(), "diff previews stay read-only");
        assert_eq!(
            std::fs::read_to_string(root.join("change.txt")).unwrap(),
            "after\n"
        );

        app.open_reference_preview(&root, DrawerRef::Commit("HEAD".into()));
        app.on_preview_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
        assert!(app.editor.is_none(), "commit previews stay read-only");
        let (title, text) = git_reference_preview(&root, "HEAD");
        assert!(title.contains("HEAD"));
        assert!(text.contains("preview commit"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_preview_keeps_search_line_and_escape_returns() {
        assert_eq!(Preview::new("t".into(), "one".into(), true, 99).anchor, 0);
        let root = temp_dir("preview");
        std::fs::write(root.join("file.txt"), "one\ntwo\nthree\n").unwrap();
        let mut app = App::new(root.clone(), None);
        app.open_file_preview(&root, Path::new("file.txt"), None, 1);
        assert_eq!(app.preview.as_ref().unwrap().anchor, 1);
        app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.preview.as_ref().unwrap().anchor, 2);
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.preview.is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn activity_keys_switch_to_reference_views() {
        let root = temp_dir("views");
        let mut app = App::new(root.clone(), None);
        assert!(!app.on_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE)));
        assert!(app.view == View::Search);
        assert!(!app.on_key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::CONTROL)));
        assert!(app.view == View::SourceControl);
        std::fs::remove_dir_all(root).unwrap();
    }
}
