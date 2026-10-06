//! Standalone Source Control panel, including Git Graph's eight drawers.
//!
//! Integration: declare `mod scm; mod scm_view;`, construct with `new(&cwd)`,
//! call `draw` in the Source Control body, and dispatch its typed actions from
//! `on_key` / `on_mouse`. Import the host's BG/FG/MUTED/ACCENT/BORDER/SELECTED
//! palette constants. `OpenCommit` is a hash in `active_root()`; there is no
//! preview, activity bar, or separate graph view here. Git operations use the
//! sibling `crate::scm` core, including HEAD-only `graph(30)` (never `--all`).

use std::path::{Path, PathBuf};

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use unicode_width::UnicodeWidthChar;

use crate::icons::{self, IconTheme};
pub use crate::scm::Drawer;
use crate::scm::{DRAWER_LIMIT, FileEntry, Git, Status};
use crate::{ACCENT, BG, BORDER, FG, MUTED, SELECTED};

// Upstream dark-theme hover and Git decoration colors.
const BUTTON_FOCUS: Color = Color::Rgb(0x02, 0x8a, 0xf0);
const HOVER: Color = Color::Rgb(48, 52, 60);
const ERROR: Color = Color::Rgb(0xe4, 0x67, 0x6b);
const MESSAGE_MAX_ROWS: usize = 6;

/// Parent-owned navigation/opening. No operation launches a Herdr preview.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ScmAction {
    #[default]
    None,
    Quit,
    SwitchToExplorer,
    SwitchToSearch,
    /// Short commit hash, relative to `ScmView::active_root()`.
    OpenCommit(String),
    OpenFile {
        root: PathBuf,
        path: String,
        staged: bool,
    },
    OpenReference {
        root: PathBuf,
        reference: DrawerRef,
    },
}

/// The original, actionable value, independent of its shortened display text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum DrawerRef {
    #[default]
    None,
    Commit(String),
    Stash(usize),
    Branch {
        name: String,
        current: bool,
    },
    Remote {
        name: String,
        url: String,
    },
    Tag(String),
    Worktree(String),
}

#[derive(Default)]
struct DrawerPanel {
    expanded: bool,
    lines: Vec<String>,
    refs: Vec<DrawerRef>,
    error: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focus {
    Message,
    Commit,
    List,
}

struct Repo {
    git: Git,
    name: String,
    status: Status,
    error: Option<String>,
    collapsed: bool,
    staged_collapsed: bool,
    changes_collapsed: bool,
    message: Vec<char>,
    cursor: usize,
}

impl Repo {
    fn new(git: Git) -> Self {
        let mut repo = Self {
            name: git.name(),
            git,
            status: Status::default(),
            error: None,
            collapsed: false,
            staged_collapsed: false,
            changes_collapsed: false,
            message: Vec::new(),
            cursor: 0,
        };
        repo.refresh();
        repo
    }

    fn refresh(&mut self) {
        match self.git.status() {
            Ok(status) => {
                self.status = status;
                self.error = None;
            }
            Err(error) => {
                // Do not offer stale paths for staging after a failed status read.
                self.status = Status::default();
                self.error = Some(error);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Row {
    RepoHeader(usize),
    Message(usize),
    Commit(usize),
    Error(usize),
    StagedHeader(usize),
    ChangesHeader(usize),
    Staged(usize, usize),
    Unstaged(usize, usize),
    DrawerHeader(Drawer),
    DrawerLine(Drawer, usize),
}

impl Row {
    fn repo(self) -> Option<usize> {
        match self {
            Self::RepoHeader(r)
            | Self::Message(r)
            | Self::Commit(r)
            | Self::Error(r)
            | Self::StagedHeader(r)
            | Self::ChangesHeader(r)
            | Self::Staged(r, _)
            | Self::Unstaged(r, _) => Some(r),
            _ => None,
        }
    }

    fn selectable(self) -> bool {
        !matches!(self, Self::Message(_) | Self::Commit(_) | Self::Error(_))
    }
}

#[derive(Clone, Copy)]
struct RowHit {
    index: usize,
    area: Rect,
}

#[derive(Default)]
struct Zones {
    area: Rect,
    header: Rect,
    refresh: Rect,
    message: Rect,
    commit: Rect,
    list: Rect,
    rows: Vec<RowHit>,
}

/// Source Control state. Drawers follow the selected repository and file.
/// Commands are synchronous, like upstream; the caller owns the event loop.
pub struct ScmView {
    cwd: PathBuf,
    repos: Vec<Repo>,
    active: usize,
    drawers: [DrawerPanel; 8],
    history_target: Option<String>,
    rows: Vec<Row>,
    selected: Option<usize>,
    /// Scroll offset in terminal lines (inline widgets can occupy several).
    scroll: usize,
    focus: Focus,
    flash: Option<(String, bool)>,
    zones: Zones,
    mouse_pos: Option<(u16, u16)>,
    follow_cursor: bool,
}

impl ScmView {
    pub fn new(cwd: &Path) -> Self {
        let mut view = Self {
            cwd: cwd.to_path_buf(),
            repos: Git::discover_all(cwd).into_iter().map(Repo::new).collect(),
            active: 0,
            drawers: std::array::from_fn(|_| DrawerPanel::default()),
            history_target: None,
            rows: Vec::new(),
            selected: None,
            scroll: 0,
            focus: Focus::List,
            flash: None,
            zones: Zones::default(),
            mouse_pos: None,
            follow_cursor: true,
        };
        view.rebuild();
        view
    }

    pub fn active_root(&self) -> Option<&Path> {
        self.repos.get(self.active).map(|repo| repo.git.root())
    }

    /// Rediscover repositories, preserve their inputs/folds, update status and
    /// only expanded drawers. Explicit refresh also clears old operation errors.
    pub fn refresh(&mut self) {
        self.flash = None;
        self.reload();
    }

    fn reload(&mut self) {
        let selected = self.selected.and_then(|i| self.rows.get(i)).copied();
        let active_root = self.active_root().map(Path::to_path_buf);
        let mut old = std::mem::take(&mut self.repos);
        self.repos = Git::discover_all(&self.cwd)
            .into_iter()
            .map(|git| {
                if let Some(i) = old.iter().position(|repo| repo.git.root() == git.root()) {
                    let mut repo = old.remove(i);
                    repo.refresh();
                    repo
                } else {
                    Repo::new(git)
                }
            })
            .collect();
        self.active = active_root
            .as_deref()
            .and_then(|root| self.repos.iter().position(|repo| repo.git.root() == root))
            .unwrap_or(0);
        if self.active_root() != active_root.as_deref() {
            self.history_target = None;
        }
        self.reload_expanded_drawers();
        self.rebuild();
        // Drawer rows remain stable when a staged section appears/disappears.
        if let Some(row @ (Row::DrawerHeader(_) | Row::DrawerLine(_, _))) = selected {
            self.selected = self.rows.iter().position(|candidate| *candidate == row);
        }
        self.follow_selection();
        self.follow_cursor = true;
    }

    fn reload_expanded_drawers(&mut self) {
        for kind in Drawer::ALL {
            if self.drawers[drawer_index(kind)].expanded {
                self.load_drawer(kind);
            }
        }
    }

    fn load_drawer(&mut self, kind: Drawer) {
        let history_target = self.history_target.as_deref();
        let result = self
            .repos
            .get(self.active)
            .ok_or_else(|| "No Git repository found".to_string())
            .and_then(|repo| repo.git.drawer_lines(kind, history_target, DRAWER_LIMIT));
        let panel = &mut self.drawers[drawer_index(kind)];
        panel.error = result.is_err();
        let raw = match result {
            Ok(lines) if lines.is_empty() => vec!["(none)".to_string()],
            Ok(lines) => lines,
            Err(error) => vec![format!("({error})")],
        };
        panel.refs = raw
            .iter()
            .map(|line| {
                if panel.error || line == "(none)" || line == "(select a file above)" {
                    DrawerRef::None
                } else {
                    parse_drawer_ref(kind, line)
                }
            })
            .collect();
        panel.lines = raw
            .into_iter()
            .map(|line| {
                if panel.error || line.starts_with('(') {
                    return line;
                }
                match kind {
                    Drawer::Worktrees => pretty_worktree_line(&line),
                    Drawer::Remotes => pretty_remote_line(&line),
                    _ => line,
                }
            })
            .collect();
    }

    fn rebuild(&mut self) {
        self.rows.clear();
        let multi = self.repos.len() > 1;
        for (r, repo) in self.repos.iter().enumerate() {
            if multi {
                self.rows.push(Row::RepoHeader(r));
                if repo.collapsed {
                    continue;
                }
                self.rows.push(Row::Message(r));
                self.rows.push(Row::Commit(r));
            } else if repo.collapsed {
                continue;
            }
            if repo.error.is_some() {
                self.rows.push(Row::Error(r));
            }
            // Upstream only shows Staged Changes when something is staged.
            if !repo.status.staged.is_empty() {
                self.rows.push(Row::StagedHeader(r));
                if !repo.staged_collapsed {
                    self.rows
                        .extend((0..repo.status.staged.len()).map(|i| Row::Staged(r, i)));
                }
            }
            self.rows.push(Row::ChangesHeader(r));
            if !repo.changes_collapsed {
                self.rows
                    .extend((0..repo.status.unstaged.len()).map(|i| Row::Unstaged(r, i)));
            }
        }
        for kind in Drawer::ALL {
            self.rows.push(Row::DrawerHeader(kind));
            let panel = &self.drawers[drawer_index(kind)];
            if panel.expanded {
                self.rows
                    .extend((0..panel.lines.len()).map(|i| Row::DrawerLine(kind, i)));
            }
        }
        if let Some(i) = self.selected {
            self.selected = self.nearest_selectable(i.min(self.rows.len().saturating_sub(1)));
        }
        // Row geometry is no longer valid until the next draw.
        self.zones.rows.clear();
        self.follow_cursor = true;
    }

    fn nearest_selectable(&self, from: usize) -> Option<usize> {
        (from..self.rows.len())
            .find(|&i| self.rows[i].selectable())
            .or_else(|| (0..from).rev().find(|&i| self.rows[i].selectable()))
    }

    fn select(&mut self, i: usize) {
        self.selected = self.nearest_selectable(i.min(self.rows.len().saturating_sub(1)));
        self.focus = Focus::List;
        self.follow_selection();
        self.follow_cursor = true;
    }

    fn follow_selection(&mut self) {
        let row = self.selected.and_then(|i| self.rows.get(i)).copied();
        if let Some(r) = row.and_then(Row::repo)
            && r != self.active
            && r < self.repos.len()
        {
            self.active = r;
            self.history_target = None;
            self.reload_expanded_drawers();
            self.rebuild();
        }
        let entry = match row {
            Some(Row::Staged(r, i)) => self.repos[r].status.staged.get(i),
            Some(Row::Unstaged(r, i)) => self.repos[r].status.unstaged.get(i),
            _ => None,
        };
        if let Some(path) = entry.map(|entry| entry.path.clone())
            && self.history_target.as_ref() != Some(&path)
        {
            self.history_target = Some(path);
            if self.drawers[drawer_index(Drawer::FileHistory)].expanded {
                self.load_drawer(Drawer::FileHistory);
                self.rebuild();
            }
        }
    }

    fn move_by(&mut self, delta: isize) {
        let Some(current) = self.selected else {
            self.select(0);
            return;
        };
        let last = self.rows.len().saturating_sub(1) as isize;
        let target = (current as isize + delta).clamp(0, last) as usize;
        let next = if delta < 0 {
            (0..=target).rev().find(|&i| self.rows[i].selectable())
        } else {
            (target..self.rows.len()).find(|&i| self.rows[i].selectable())
        };
        self.select(next.unwrap_or(current));
    }

    pub fn on_key(&mut self, key: KeyEvent) -> ScmAction {
        if key.kind == KeyEventKind::Release {
            return ScmAction::None;
        }
        let shortcut = key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER)
            && !key.modifiers.contains(KeyModifiers::ALT);
        if shortcut {
            match key.code {
                KeyCode::Char('1') => return ScmAction::SwitchToExplorer,
                KeyCode::Char('2') => return ScmAction::SwitchToSearch,
                KeyCode::Char('3') => return ScmAction::None,
                KeyCode::Char('r' | 'R') => {
                    self.refresh();
                    return ScmAction::None;
                }
                KeyCode::Enter => {
                    self.commit();
                    return ScmAction::None;
                }
                _ => {}
            }
        }
        if key.code == KeyCode::F(5) {
            self.refresh();
            return ScmAction::None;
        }
        match self.focus {
            Focus::Message => self.on_message_key(key),
            Focus::Commit => match key.code {
                KeyCode::Enter | KeyCode::Char(' ') => self.commit(),
                KeyCode::Esc | KeyCode::Tab | KeyCode::Down => {
                    self.focus = Focus::List;
                    self.follow_cursor = true;
                }
                KeyCode::BackTab | KeyCode::Up => {
                    self.focus = Focus::Message;
                    self.follow_cursor = true;
                }
                _ => {}
            },
            Focus::List => match key.code {
                KeyCode::Char('q') => return ScmAction::Quit,
                KeyCode::Esc | KeyCode::Char('1') => return ScmAction::SwitchToExplorer,
                KeyCode::Char('2') => return ScmAction::SwitchToSearch,
                KeyCode::Tab | KeyCode::Char('c') => {
                    self.focus = Focus::Message;
                    self.follow_cursor = true;
                }
                KeyCode::BackTab => {
                    self.focus = Focus::Commit;
                    self.follow_cursor = true;
                }
                KeyCode::Up | KeyCode::Char('k') => self.move_by(-1),
                KeyCode::Down | KeyCode::Char('j') => self.move_by(1),
                KeyCode::PageUp => self.move_by(-(self.zones.list.height.max(1) as isize)),
                KeyCode::PageDown => self.move_by(self.zones.list.height.max(1) as isize),
                KeyCode::Home | KeyCode::Char('g') => self.select(0),
                KeyCode::End | KeyCode::Char('G') => self.select(self.rows.len().saturating_sub(1)),
                KeyCode::Enter | KeyCode::Char(' ') => return self.activate(),
                KeyCode::Left => self.fold_selected(false),
                KeyCode::Right => self.fold_selected(true),
                KeyCode::Char('a') => self.stage_all(),
                KeyCode::Char('u') => self.unstage_all(),
                KeyCode::Char('+' | '-') => return self.activate(),
                KeyCode::Char('r') => self.refresh(),
                KeyCode::Char('o') => return self.open_file(),
                _ => {}
            },
        }
        ScmAction::None
    }

    fn on_message_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {
                self.commit();
                return;
            }
            KeyCode::Esc | KeyCode::BackTab => {
                self.focus = Focus::List;
                self.follow_cursor = true;
                return;
            }
            KeyCode::Tab | KeyCode::Down => {
                self.focus = Focus::Commit;
                self.follow_cursor = true;
                return;
            }
            _ => {}
        }
        let Some(repo) = self.repos.get_mut(self.active) else {
            return;
        };
        match key.code {
            KeyCode::Backspace if repo.cursor > 0 => {
                repo.cursor -= 1;
                repo.message.remove(repo.cursor);
            }
            KeyCode::Delete if repo.cursor < repo.message.len() => {
                repo.message.remove(repo.cursor);
            }
            KeyCode::Left => repo.cursor = repo.cursor.saturating_sub(1),
            KeyCode::Right => repo.cursor = (repo.cursor + 1).min(repo.message.len()),
            KeyCode::Home => repo.cursor = 0,
            KeyCode::End => repo.cursor = repo.message.len(),
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                repo.message.clear();
                repo.cursor = 0;
            }
            KeyCode::Char(c)
                if !c.is_control()
                    && (!key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER)
                        || key.modifiers.contains(KeyModifiers::ALT)) =>
            {
                repo.message.insert(repo.cursor, c);
                repo.cursor += 1;
            }
            _ => {}
        }
        self.follow_cursor = true;
    }

    fn activate(&mut self) -> ScmAction {
        let Some(row) = self.selected.and_then(|i| self.rows.get(i)).copied() else {
            return ScmAction::None;
        };
        match row {
            Row::RepoHeader(r) => self.repos[r].collapsed = !self.repos[r].collapsed,
            Row::StagedHeader(r) => {
                self.repos[r].staged_collapsed = !self.repos[r].staged_collapsed
            }
            Row::ChangesHeader(r) => {
                self.repos[r].changes_collapsed = !self.repos[r].changes_collapsed
            }
            Row::DrawerHeader(kind) => {
                let panel = &mut self.drawers[drawer_index(kind)];
                panel.expanded = !panel.expanded;
                if panel.expanded {
                    self.load_drawer(kind);
                }
            }
            Row::DrawerLine(kind, i) => {
                let reference = self.drawers[drawer_index(kind)]
                    .refs
                    .get(i)
                    .cloned()
                    .unwrap_or_default();
                return match reference {
                    DrawerRef::None => ScmAction::None,
                    DrawerRef::Commit(hash) => ScmAction::OpenCommit(hash),
                    reference => match self.active_root() {
                        Some(root) => ScmAction::OpenReference {
                            root: root.to_path_buf(),
                            reference,
                        },
                        None => ScmAction::None,
                    },
                };
            }
            Row::Staged(r, i) => {
                let repo = &self.repos[r];
                self.finish_op(repo.git.unstage(&repo.status.staged[i]));
                return ScmAction::None;
            }
            Row::Unstaged(r, i) => {
                let repo = &self.repos[r];
                self.finish_op(repo.git.stage(&repo.status.unstaged[i]));
                return ScmAction::None;
            }
            _ => return ScmAction::None,
        }
        self.rebuild();
        ScmAction::None
    }

    fn fold_selected(&mut self, expanded: bool) {
        let Some(row) = self.selected.and_then(|i| self.rows.get(i)).copied() else {
            return;
        };
        match row {
            Row::RepoHeader(r) => self.repos[r].collapsed = !expanded,
            Row::StagedHeader(r) => self.repos[r].staged_collapsed = !expanded,
            Row::ChangesHeader(r) => self.repos[r].changes_collapsed = !expanded,
            Row::DrawerHeader(kind) => {
                let panel = &mut self.drawers[drawer_index(kind)];
                let load = expanded && !panel.expanded;
                panel.expanded = expanded;
                if load {
                    self.load_drawer(kind);
                }
            }
            Row::DrawerLine(kind, _) if !expanded => {
                if let Some(i) = self
                    .rows
                    .iter()
                    .position(|row| *row == Row::DrawerHeader(kind))
                {
                    self.select(i);
                }
                return;
            }
            _ => return,
        }
        self.rebuild();
    }

    fn open_file(&self) -> ScmAction {
        let Some(row) = self.selected.and_then(|i| self.rows.get(i)).copied() else {
            return ScmAction::None;
        };
        let (r, i, staged) = match row {
            Row::Staged(r, i) => (r, i, true),
            Row::Unstaged(r, i) => (r, i, false),
            _ => return ScmAction::None,
        };
        let repo = &self.repos[r];
        let entry = if staged {
            &repo.status.staged[i]
        } else {
            &repo.status.unstaged[i]
        };
        ScmAction::OpenFile {
            root: repo.git.root().to_path_buf(),
            path: entry.path.clone(),
            staged,
        }
    }

    fn finish_op(&mut self, result: Result<(), String>) {
        self.flash = result.err().map(|error| (error, true));
        // reload, not refresh: refresh would erase the operation's error.
        self.reload();
    }

    pub fn stage_all(&mut self) {
        let result = self
            .repos
            .get(self.active)
            .map(|repo| repo.git.stage_all())
            .unwrap_or_else(|| Err("No Git repository found".to_string()));
        self.finish_op(result);
    }

    pub fn unstage_all(&mut self) {
        let result = self
            .repos
            .get(self.active)
            .map(|repo| repo.git.unstage_all())
            .unwrap_or_else(|| Err("No Git repository found".to_string()));
        self.finish_op(result);
    }

    pub fn commit(&mut self) {
        let Some(repo) = self.repos.get(self.active) else {
            self.flash = Some(("No Git repository found".to_string(), true));
            return;
        };
        let message: String = repo.message.iter().collect();
        let result = if message.trim().is_empty() {
            Err("Enter a commit message".to_string())
        } else if repo.status.staged.is_empty() {
            Err("No staged changes to commit".to_string())
        } else {
            repo.git.commit(&message)
        };
        match result {
            Ok(summary) => {
                self.repos[self.active].message.clear();
                self.repos[self.active].cursor = 0;
                self.flash = Some((summary, false));
                self.focus = Focus::List;
            }
            Err(error) => self.flash = Some((error, true)),
        }
        self.reload();
    }

    pub fn on_mouse(&mut self, mouse: MouseEvent) -> ScmAction {
        let (x, y) = (mouse.column, mouse.row);
        if !hits(self.zones.area, x, y) {
            return ScmAction::None;
        }
        self.mouse_pos = Some((x, y));
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.scroll = self.scroll.saturating_sub(3);
                self.follow_cursor = false;
            }
            MouseEventKind::ScrollDown => {
                self.scroll = self.scroll.saturating_add(3);
                self.follow_cursor = false;
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if hits(self.zones.refresh, x, y) {
                    self.refresh();
                    return ScmAction::None;
                }
                if hits(self.zones.message, x, y) {
                    self.focus = Focus::Message;
                    self.follow_cursor = true;
                    return ScmAction::None;
                }
                if hits(self.zones.commit, x, y) {
                    self.focus = Focus::Commit;
                    self.commit();
                    return ScmAction::None;
                }
                if self.repos.len() == 1 && hits(self.zones.header, x, y) {
                    self.repos[0].collapsed = !self.repos[0].collapsed;
                    self.rebuild();
                    return ScmAction::None;
                }
                if let Some(hit) = self
                    .zones
                    .rows
                    .iter()
                    .find(|hit| hits(hit.area, x, y))
                    .copied()
                {
                    let row = self.rows[hit.index];
                    let rel_x = x.saturating_sub(hit.area.x) as usize;
                    match row {
                        Row::Message(r) => {
                            self.set_active(r);
                            self.focus = Focus::Message;
                            self.follow_cursor = true;
                        }
                        Row::Commit(r) => {
                            self.set_active(r);
                            self.focus = Focus::Commit;
                            self.commit();
                        }
                        Row::Error(_) => {}
                        Row::Staged(_, _) | Row::Unstaged(_, _) => {
                            self.select(hit.index);
                            // Click the visible +/- action to stage, the name to open.
                            if file_action_start(hit.area.width as usize)
                                .is_some_and(|start| (start..start + 3).contains(&rel_x))
                            {
                                return self.activate();
                            }
                            return self.open_file();
                        }
                        Row::StagedHeader(r) | Row::ChangesHeader(r) => {
                            self.select(hit.index);
                            let count = if matches!(row, Row::StagedHeader(_)) {
                                self.repos[r].status.staged.len()
                            } else {
                                self.repos[r].status.unstaged.len()
                            };
                            if section_action_start(hit.area.width as usize, count)
                                .is_some_and(|start| (start..start + 2).contains(&rel_x))
                            {
                                if matches!(row, Row::StagedHeader(_)) {
                                    self.unstage_all();
                                } else {
                                    self.stage_all();
                                }
                            } else {
                                return self.activate();
                            }
                        }
                        _ => {
                            self.select(hit.index);
                            return self.activate();
                        }
                    }
                }
            }
            _ => {}
        }
        ScmAction::None
    }

    fn set_active(&mut self, r: usize) {
        if r != self.active {
            self.active = r;
            // A clicked input belongs to this repo, even if the previously
            // selected file was in another one. Reload must not switch back.
            self.selected = self.rows.iter().position(|row| *row == Row::RepoHeader(r));
            self.history_target = None;
            self.reload_expanded_drawers();
            self.rebuild();
        }
    }

    pub fn draw(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        theme: IconTheme,
        mouse_pos: Option<(u16, u16)>,
    ) {
        self.mouse_pos = mouse_pos;
        self.zones = Zones {
            area,
            ..Zones::default()
        };
        frame.render_widget(Block::default().style(Style::default().bg(BG).fg(FG)), area);
        if area.is_empty() {
            return;
        }
        let single = self.repos.len() == 1;
        let controls = single && !self.repos[0].collapsed;
        let message_height = if controls {
            self.message_height(0, area.width) as u16
        } else {
            0
        };
        let [header, message, button, list, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(message_height),
            Constraint::Length(if controls { 3 } else { 0 }),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .areas(area);
        self.zones.header = header;
        self.zones.list = list;
        self.draw_header(frame, header, theme);
        if controls {
            self.zones.message = message;
            self.zones.commit = button;
            self.draw_message(frame, message, 0, 0);
            self.draw_button(frame, button, 0, 0);
        }
        self.draw_list(frame, list, theme);
        let (text, style) = if let Some((message, error)) = &self.flash {
            (
                format!(" {}", one_line(message)),
                Style::default().fg(if *error { ERROR } else { FG }),
            )
        } else if self.repos.is_empty() {
            (
                " No Git repository found · r refresh".to_string(),
                Style::default().fg(ERROR),
            )
        } else {
            (
                " Enter stage/open · a stage all · u unstage all · c commit · r refresh"
                    .to_string(),
                Style::default().fg(MUTED),
            )
        };
        frame.render_widget(Paragraph::new(text).style(style), footer);
    }

    fn draw_header(&mut self, frame: &mut Frame<'_>, area: Rect, theme: IconTheme) {
        let single = self.repos.len() == 1;
        let left = if single {
            let repo = &self.repos[0];
            format!(" {} {}", if repo.collapsed { "▸" } else { "▾" }, repo.name)
        } else {
            " ▾ Source Control".to_string()
        };
        let right = self
            .repos
            .get(self.active)
            .map(|repo| {
                let prefix = if single {
                    String::new()
                } else {
                    format!("{} · ", repo.name)
                };
                format!(
                    "{prefix}{} {}{}",
                    branch_icon(theme),
                    repo.status.branch,
                    sync_counts(&repo.status)
                )
            })
            .unwrap_or_default();
        let refresh_width = area.width.min(3);
        self.zones.refresh = Rect::new(
            area.right().saturating_sub(refresh_width),
            area.y,
            refresh_width,
            area.height,
        );
        let width = area.width.saturating_sub(refresh_width) as usize;
        frame.render_widget(
            Paragraph::new(aligned_line(left, right, width, true)),
            Rect::new(area.x, area.y, width as u16, area.height),
        );
        let hovered = self
            .mouse_pos
            .is_some_and(|(x, y)| hits(self.zones.refresh, x, y));
        frame.render_widget(
            Paragraph::new(" ↻ ").style(Style::default().fg(MUTED).bg(if hovered {
                HOVER
            } else {
                BG
            })),
            self.zones.refresh,
        );
    }

    fn row_height(&self, row: Row, width: u16) -> usize {
        match row {
            Row::Message(r) => self.message_height(r, width),
            Row::Commit(_) => 3,
            _ => 1,
        }
    }

    fn message_height(&self, r: usize, width: u16) -> usize {
        let repo = &self.repos[r];
        let (lines, _, _) = wrap_message(
            &repo.message,
            repo.cursor,
            width.saturating_sub(2).max(1) as usize,
        );
        lines.len().clamp(1, MESSAGE_MAX_ROWS) + 2
    }

    fn draw_list(&mut self, frame: &mut Frame<'_>, area: Rect, theme: IconTheme) {
        let heights: Vec<usize> = self
            .rows
            .iter()
            .map(|row| self.row_height(*row, area.width))
            .collect();
        let total: usize = heights.iter().sum();
        let visible = area.height as usize;
        if self.follow_cursor && visible > 0 {
            let target = match self.focus {
                Focus::List => self.selected,
                Focus::Message => self
                    .rows
                    .iter()
                    .position(|row| *row == Row::Message(self.active)),
                Focus::Commit => self
                    .rows
                    .iter()
                    .position(|row| *row == Row::Commit(self.active)),
            };
            if let Some(i) = target {
                let top: usize = heights.iter().take(i).sum();
                let bottom = top + heights[i];
                if top < self.scroll {
                    self.scroll = top;
                }
                if bottom > self.scroll + visible {
                    self.scroll = bottom.saturating_sub(visible).min(top);
                }
            }
            self.follow_cursor = false;
        }
        self.scroll = self.scroll.min(total.saturating_sub(visible));
        let mut top = 0;
        for (i, height) in heights.into_iter().enumerate() {
            let bottom = top + height;
            if bottom > self.scroll && top < self.scroll + visible {
                let clipped = self.scroll.saturating_sub(top);
                let y = area.y + top.saturating_sub(self.scroll) as u16;
                let shown = (height - clipped).min((area.bottom() - y) as usize);
                let rect = Rect::new(area.x, y, area.width, shown as u16);
                self.zones.rows.push(RowHit {
                    index: i,
                    area: rect,
                });
                match self.rows[i] {
                    Row::Message(r) => self.draw_message(frame, rect, r, clipped),
                    Row::Commit(r) => self.draw_button(frame, rect, r, clipped),
                    row => {
                        let hovered = self.mouse_pos.is_some_and(|(x, y)| hits(rect, x, y));
                        let selected = self.selected == Some(i) && self.focus == Focus::List;
                        let style = Style::default().fg(FG).bg(if selected {
                            SELECTED
                        } else if hovered {
                            HOVER
                        } else {
                            BG
                        });
                        frame.render_widget(
                            Paragraph::new(self.row_line(row, area.width as usize, theme, hovered))
                                .style(style),
                            rect,
                        );
                    }
                }
            }
            top = bottom;
        }
    }

    fn row_line(&self, row: Row, width: usize, theme: IconTheme, hovered: bool) -> Line<'static> {
        match row {
            Row::RepoHeader(r) => {
                let repo = &self.repos[r];
                let icon = icons::icon(theme, "", true, !repo.collapsed);
                aligned_line(
                    format!(
                        " {} {} {}",
                        if repo.collapsed { "▸" } else { "▾" },
                        icon.glyph,
                        repo.name
                    ),
                    format!(
                        "{} {}{} ",
                        branch_icon(theme),
                        repo.status.branch,
                        sync_counts(&repo.status)
                    ),
                    width,
                    r == self.active,
                )
            }
            Row::Error(r) => Line::styled(
                format!(
                    "   {}",
                    one_line(self.repos[r].error.as_deref().unwrap_or("Git error"))
                ),
                Style::default().fg(ERROR),
            ),
            Row::StagedHeader(r) => section_line(
                "Staged Changes",
                self.repos[r].staged_collapsed,
                Some(self.repos[r].status.staged.len()),
                width,
                hovered.then_some('−'),
            ),
            Row::ChangesHeader(r) => section_line(
                "Changes",
                self.repos[r].changes_collapsed,
                Some(self.repos[r].status.unstaged.len()),
                width,
                hovered.then_some('+'),
            ),
            Row::Staged(r, i) => {
                file_line(&self.repos[r].status.staged[i], width, theme, true, hovered)
            }
            Row::Unstaged(r, i) => file_line(
                &self.repos[r].status.unstaged[i],
                width,
                theme,
                false,
                hovered,
            ),
            Row::DrawerHeader(kind) => {
                let mut line = section_line(
                    kind.title(),
                    !self.drawers[drawer_index(kind)].expanded,
                    None,
                    width,
                    None,
                );
                if kind == Drawer::FileHistory
                    && let Some(path) = &self.history_target
                {
                    line.spans.push(Span::styled(
                        format!("  {}", path.rsplit('/').next().unwrap_or(path)),
                        Style::default().fg(MUTED),
                    ));
                }
                line
            }
            Row::DrawerLine(kind, i) => {
                let panel = &self.drawers[drawer_index(kind)];
                let text = &panel.lines[i];
                let style = if panel.error {
                    Style::default().fg(ERROR)
                } else if text.starts_with('(') {
                    Style::default().fg(MUTED)
                } else if kind == Drawer::Branches && text.starts_with('*') {
                    Style::default().fg(status_color('U')).bold()
                } else {
                    Style::default()
                };
                // Preserve graph connectors and Git's indentation verbatim.
                Line::styled(format!("   {}", one_line(text)), style)
            }
            _ => Line::default(),
        }
    }

    fn draw_message(&self, frame: &mut Frame<'_>, area: Rect, r: usize, clipped: usize) {
        if area.is_empty() {
            return;
        }
        let repo = &self.repos[r];
        let focused = self.active == r && self.focus == Focus::Message;
        let field = area.width.saturating_sub(2).max(1) as usize;
        let (rows, cursor_row, cursor_col) = wrap_message(&repo.message, repo.cursor, field);
        let count = rows.len().clamp(1, MESSAGE_MAX_ROWS);
        let window = if focused {
            cursor_row.saturating_sub(count - 1)
        } else {
            0
        };
        let border_style = Style::default().fg(if focused { ACCENT } else { BORDER });
        let horizontal = "─".repeat(area.width.saturating_sub(2) as usize);
        let mut lines = vec![Line::styled(format!("┌{horizontal}┐"), border_style)];
        for line in rows.iter().skip(window).take(count) {
            let text = if repo.message.is_empty() && !focused {
                clip("Message (Enter to commit)", field)
            } else {
                line.clone()
            };
            let pad = field.saturating_sub(Span::raw(&text).width());
            lines.push(Line::from(vec![
                Span::styled("│", border_style),
                Span::styled(
                    text,
                    Style::default().fg(if repo.message.is_empty() && !focused {
                        MUTED
                    } else {
                        FG
                    }),
                ),
                Span::raw(" ".repeat(pad)),
                Span::styled("│", border_style),
            ]));
        }
        lines.push(Line::styled(format!("└{horizontal}┘"), border_style));
        frame.render_widget(Paragraph::new(lines).scroll((clipped as u16, 0)), area);
        let cursor_y = cursor_row.saturating_sub(window) + 1;
        if focused
            && area.width > 2
            && cursor_y >= clipped
            && cursor_y - clipped < area.height as usize
        {
            frame.set_cursor_position(Position::new(
                area.x + 1 + cursor_col.min(field - 1) as u16,
                area.y + (cursor_y - clipped) as u16,
            ));
        }
    }

    fn draw_button(&self, frame: &mut Frame<'_>, area: Rect, r: usize, clipped: usize) {
        let focused = self.active == r && self.focus == Focus::Commit;
        let bg = if focused {
            BUTTON_FOCUS
        } else if self.active == r {
            ACCENT
        } else {
            Color::Rgb(0x24, 0x45, 0x5c)
        };
        if area.width < 2 {
            return;
        }
        let width = area.width.saturating_sub(2) as usize;
        let half = "▀".repeat(width);
        let label = clip("✓ Commit", width);
        let left_pad = width.saturating_sub(Span::raw(&label).width()) / 2;
        let right_pad = width.saturating_sub(left_pad + Span::raw(&label).width());
        let style = Style::default()
            .bg(bg)
            .fg(Color::White)
            .add_modifier(if focused {
                Modifier::BOLD
            } else {
                Modifier::empty()
            });
        let lines = vec![
            Line::from(vec![
                Span::raw(" "),
                Span::styled("▄".repeat(width), Style::default().fg(bg)),
                Span::raw(" "),
            ]),
            Line::from(vec![
                Span::raw(" "),
                Span::styled(
                    format!("{}{label}{}", " ".repeat(left_pad), " ".repeat(right_pad)),
                    style,
                ),
                Span::raw(" "),
            ]),
            Line::from(vec![
                Span::raw(" "),
                Span::styled(half, Style::default().fg(bg)),
                Span::raw(" "),
            ]),
        ];
        frame.render_widget(Paragraph::new(lines).scroll((clipped as u16, 0)), area);
    }
}

fn drawer_index(kind: Drawer) -> usize {
    Drawer::ALL
        .iter()
        .position(|candidate| *candidate == kind)
        .unwrap_or(0)
}

fn hits(rect: Rect, x: u16, y: u16) -> bool {
    rect.contains(Position::new(x, y))
}

fn branch_icon(theme: IconTheme) -> &'static str {
    match theme {
        IconTheme::Material => "\u{e725}",
        IconTheme::Emoji => "⎇",
    }
}

fn sync_counts(status: &Status) -> String {
    if status.ahead + status.behind > 0 {
        format!(" {}↑ {}↓", status.ahead, status.behind)
    } else {
        String::new()
    }
}

fn status_color(letter: char) -> Color {
    match letter {
        'M' => Color::Rgb(0xe2, 0xc0, 0x8d),
        'U' | 'R' | 'C' => Color::Rgb(0x73, 0xc9, 0x91),
        'A' => Color::Rgb(0x81, 0xb8, 0x8b),
        'D' => Color::Rgb(0xc7, 0x4e, 0x39),
        '!' => ERROR,
        _ => MUTED,
    }
}

/// Escape control characters from Git paths/errors without damaging graph spaces.
fn one_line(text: &str) -> String {
    text.chars()
        .flat_map(|c| {
            if c.is_control() {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}

fn clip(text: &str, width: usize) -> String {
    let mut used = 0;
    one_line(text)
        .chars()
        .take_while(|c| {
            used += c.width().unwrap_or(0);
            used <= width
        })
        .collect()
}

fn aligned_line(left: String, right: String, width: usize, bold: bool) -> Line<'static> {
    let right = clip(&right, width / 2);
    let rw = Span::raw(&right).width();
    let left = clip(&left, width.saturating_sub(rw));
    let pad = width.saturating_sub(rw + Span::raw(&left).width());
    Line::from(vec![
        Span::styled(
            left,
            Style::default().add_modifier(if bold {
                Modifier::BOLD
            } else {
                Modifier::empty()
            }),
        ),
        Span::raw(" ".repeat(pad)),
        Span::styled(right, Style::default().fg(MUTED)),
    ])
}

fn section_action_start(width: usize, count: usize) -> Option<usize> {
    let reserved = format!(" {count} ").len() + 1;
    (width >= 24 + reserved + 2).then(|| width - reserved - 2)
}

fn section_line(
    title: &str,
    collapsed: bool,
    count: Option<usize>,
    width: usize,
    action: Option<char>,
) -> Line<'static> {
    let left = format!(" {} {title}", if collapsed { "▸" } else { "▾" });
    let Some(count) = count else {
        return Line::styled(left, Style::default().bold());
    };
    let badge = format!(" {count} ");
    let show_action = action.is_some() && section_action_start(width, count).is_some();
    let reserved = badge.len() + 1 + usize::from(show_action) * 2;
    let left = clip(&left, width.saturating_sub(reserved));
    let pad = width.saturating_sub(Span::raw(&left).width() + reserved);
    let mut spans = vec![
        Span::styled(left, Style::default().bold()),
        Span::raw(" ".repeat(pad)),
    ];
    if show_action {
        spans.push(Span::styled(
            format!("{} ", action.unwrap()),
            Style::default().bold(),
        ));
    }
    spans.push(Span::styled(
        badge,
        Style::default().bg(ACCENT).fg(Color::White),
    ));
    spans.push(Span::raw(" "));
    Line::from(spans)
}

fn file_action_start(width: usize) -> Option<usize> {
    (width >= 12).then(|| width - 5)
}

fn file_line(
    entry: &FileEntry,
    width: usize,
    theme: IconTheme,
    staged: bool,
    hovered: bool,
) -> Line<'static> {
    let (dir, name) = entry
        .path
        .rsplit_once('/')
        .map(|(dir, name)| (Some(dir), name))
        .unwrap_or((None, &entry.path));
    let icon = icons::icon(theme, name, false, false);
    let color = status_color(entry.letter);
    let show_action = hovered && file_action_start(width).is_some();
    let tail = 2 + usize::from(show_action) * 3;
    let prefix = format!("   {} ", icon.glyph);
    let mut spans = vec![Span::styled(
        prefix,
        icon.rgb
            .map(|(r, g, b)| Style::default().fg(Color::Rgb(r, g, b)))
            .unwrap_or_default(),
    )];
    let used = spans.iter().map(Span::width).sum::<usize>();
    let name = clip(name, width.saturating_sub(used + tail));
    spans.push(Span::styled(name, Style::default().fg(color)));
    if let Some(dir) = dir {
        let used = spans.iter().map(Span::width).sum::<usize>();
        spans.push(Span::styled(
            clip(&format!(" {dir}"), width.saturating_sub(used + tail)),
            Style::default().fg(MUTED),
        ));
    }
    let used = spans.iter().map(Span::width).sum::<usize>();
    spans.push(Span::raw(" ".repeat(width.saturating_sub(used + tail))));
    if show_action {
        spans.push(Span::styled(
            if staged { " − " } else { " + " },
            Style::default().bold(),
        ));
    }
    spans.push(Span::styled(
        entry.letter.to_string(),
        Style::default().fg(color).bold(),
    ));
    spans.push(Span::raw(" "));
    Line::from(spans)
}

/// Unicode-safe editing uses char indices; wrapping/cursor placement uses cells.
fn wrap_message(message: &[char], cursor: usize, field: usize) -> (Vec<String>, usize, usize) {
    let field = field.max(1);
    let mut rows = vec![String::new()];
    let (mut row, mut col) = (0, 0);
    let (mut cursor_row, mut cursor_col) = (0, 0);
    for (i, c) in message.iter().enumerate() {
        let width = c.width().unwrap_or(0).min(field);
        if *c == '\n' || col + width > field {
            rows.push(String::new());
            row += 1;
            col = 0;
        }
        if i == cursor {
            cursor_row = row;
            cursor_col = col;
        }
        if *c != '\n' {
            rows[row].push(*c);
            col += width;
        }
        if col >= field {
            rows.push(String::new());
            row += 1;
            col = 0;
        }
    }
    if cursor >= message.len() {
        cursor_row = row;
        cursor_col = col;
    }
    (rows, cursor_row, cursor_col)
}

fn parse_drawer_ref(kind: Drawer, line: &str) -> DrawerRef {
    match kind {
        Drawer::Graph | Drawer::Commits | Drawer::FileHistory => {
            // Only inspect the commit position. A connector's subject must not
            // become an action just because it happens to contain a hex word.
            let rest = if kind == Drawer::Graph {
                let Some(star) = line.find('*') else {
                    return DrawerRef::None;
                };
                &line[star + 1..]
            } else {
                line
            };
            rest.trim_start_matches(|c: char| {
                c.is_whitespace() || matches!(c, '|' | '/' | '\\' | '_' | '-' | '.')
            })
            .split_whitespace()
            .next()
            .filter(|hash| {
                hash.len() >= 7
                    && hash
                        .chars()
                        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
            })
            .map(|hash| DrawerRef::Commit(hash.to_string()))
            .unwrap_or_default()
        }
        Drawer::Branches => {
            let name = line.trim_start_matches('*').trim();
            if name.is_empty() || name.starts_with('(') {
                DrawerRef::None
            } else {
                DrawerRef::Branch {
                    name: name.to_string(),
                    current: line.starts_with('*'),
                }
            }
        }
        Drawer::Remotes => {
            let mut parts = line.split_whitespace();
            parts
                .next()
                .map(|name| DrawerRef::Remote {
                    name: name.to_string(),
                    url: parts.next().unwrap_or("").to_string(),
                })
                .unwrap_or_default()
        }
        Drawer::Worktrees => line
            .split_whitespace()
            .next()
            .filter(|path| !path.starts_with('('))
            .map(|path| DrawerRef::Worktree(path.to_string()))
            .unwrap_or_default(),
        Drawer::Stashes => line
            .strip_prefix("stash@{")
            .and_then(|rest| rest.split('}').next())
            .and_then(|n| n.parse().ok())
            .map(DrawerRef::Stash)
            .unwrap_or_default(),
        Drawer::Tags => {
            let name = line.trim();
            if name.is_empty() || name.starts_with('(') {
                DrawerRef::None
            } else {
                DrawerRef::Tag(name.to_string())
            }
        }
    }
}

fn pretty_worktree_line(raw: &str) -> String {
    let path = raw.split_whitespace().next().unwrap_or("");
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    if let (Some(start), Some(end)) = (raw.find('['), raw.rfind(']'))
        && start < end
    {
        return format!("{name}  ⎇ {}", &raw[start + 1..end]);
    }
    if raw.contains("(bare)") {
        return format!("{name}  (bare)");
    }
    if raw.contains("detached") {
        return format!("{name}  (detached)");
    }
    name.to_string()
}

fn pretty_remote_line(raw: &str) -> String {
    let mut parts = raw.split_whitespace();
    match (parts.next(), parts.next()) {
        (Some(name), Some(url)) => {
            let trimmed = url.trim_end_matches('/').trim_end_matches(".git");
            let hosted = trimmed
                .split_once("://")
                .map(|(_, rest)| rest)
                .or_else(|| trimmed.strip_prefix("git@"));
            let path = if let Some(rest) = hosted {
                let rest = rest.replace(':', "/");
                rest.split_once('/')
                    .map(|(_, path)| path.to_string())
                    .unwrap_or(rest)
            } else {
                trimmed
                    .rsplit(['/', '\\'])
                    .next()
                    .unwrap_or(trimmed)
                    .to_string()
            };
            format!("{name}  {path}")
        }
        _ => raw.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Fixture(PathBuf);

    impl Fixture {
        fn empty() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "teditor-scm-view-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn repo() -> Self {
            let fixture = Self::empty();
            init(&fixture.0);
            fixture
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn git(root: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_string()
    }

    fn init(root: &Path) {
        std::fs::create_dir_all(root).unwrap();
        git(root, &["init", "-q", "-b", "main"]);
        for (key, value) in [
            ("user.name", "SCM View Test"),
            ("user.email", "view@example.invalid"),
            ("commit.gpgsign", "false"),
            ("core.hooksPath", ".git/no-test-hooks"),
        ] {
            git(root, &["config", key, value]);
        }
    }

    fn commit_file(root: &Path, name: &str, message: &str) {
        std::fs::write(root.join(name), message).unwrap();
        git(root, &["add", "--", name]);
        git(root, &["commit", "-qm", message]);
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn click(x: u16, y: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn draw(view: &mut ScmView, mouse: Option<(u16, u16)>) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(72, 40)).unwrap();
        terminal
            .draw(|frame| view.draw(frame, frame.area(), IconTheme::Emoji, mouse))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn text(buffer: &Buffer) -> String {
        (buffer.area.y..buffer.area.bottom())
            .map(|y| {
                (buffer.area.x..buffer.area.right())
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn select_row(view: &mut ScmView, row: Row) {
        let index = view
            .rows
            .iter()
            .position(|candidate| *candidate == row)
            .unwrap();
        view.select(index);
    }

    #[test]
    fn starts_with_exactly_eight_collapsed_drawers_in_upstream_order() {
        let fixture = Fixture::repo();
        let mut view = ScmView::new(&fixture.0);
        assert!(
            view.rows
                .iter()
                .all(|row| !matches!(row, Row::DrawerLine(_, _)))
        );
        let headers: Vec<_> = view
            .rows
            .iter()
            .filter_map(|row| match row {
                Row::DrawerHeader(kind) => Some(kind.title()),
                _ => None,
            })
            .collect();
        assert_eq!(
            headers,
            [
                "Graph",
                "Commits",
                "File History",
                "Branches",
                "Worktrees",
                "Remotes",
                "Stashes",
                "Tags"
            ]
        );
        let rendered = text(&draw(&mut view, None));
        assert!(rendered.contains("▸ Graph"));
        assert!(!rendered.contains("▾ Graph"));
        assert!(rendered.contains("Message (Enter to commit)"));
        assert!(rendered.contains("✓ Commit"));
        assert!(rendered.contains("▾ Changes"));
        assert!(!rendered.contains("Staged Changes"));
    }

    #[test]
    fn graph_is_a_head_only_drawer_with_connectors_and_typed_commit_activation() {
        let fixture = Fixture::repo();
        commit_file(&fixture.0, "base", "base commit");
        git(&fixture.0, &["checkout", "-qb", "topic"]);
        commit_file(&fixture.0, "topic", "topic commit");
        git(&fixture.0, &["checkout", "-q", "main"]);
        commit_file(&fixture.0, "main", "main commit");
        git(
            &fixture.0,
            &["merge", "--no-ff", "-qm", "merge topic", "topic"],
        );
        git(&fixture.0, &["checkout", "-qb", "unmerged"]);
        commit_file(&fixture.0, "hidden", "unmerged-only");
        git(&fixture.0, &["checkout", "-q", "main"]);
        let mut view = ScmView::new(&fixture.0);
        select_row(&mut view, Row::DrawerHeader(Drawer::Graph));
        assert_eq!(view.on_key(key(KeyCode::Right)), ScmAction::None);
        let panel = &view.drawers[drawer_index(Drawer::Graph)];
        assert!(panel.expanded);
        assert_eq!(
            panel.lines.join("\n"),
            git(
                &fixture.0,
                &["log", "--graph", "--oneline", "--decorate=short", "-30"]
            )
        );
        assert!(
            !panel
                .lines
                .iter()
                .any(|line| line.contains("unmerged-only"))
        );
        let connector = panel
            .refs
            .iter()
            .position(|reference| *reference == DrawerRef::None)
            .unwrap();
        select_row(&mut view, Row::DrawerLine(Drawer::Graph, connector));
        assert_eq!(view.on_key(key(KeyCode::Enter)), ScmAction::None);
        select_row(&mut view, Row::DrawerLine(Drawer::Graph, 0));
        let hash = git(&fixture.0, &["rev-parse", "--short", "HEAD"]);
        assert_eq!(
            view.on_key(key(KeyCode::Enter)),
            ScmAction::OpenCommit(hash)
        );
        let rendered = text(&draw(&mut view, None));
        assert!(rendered.contains("▾ Graph"));
        assert!(rendered.contains("   *"));
        assert!(rendered.contains("merge topic"));
        assert!(rendered.contains("▸ Commits"));
        view.on_key(key(KeyCode::Left)); // parent header, then collapse
        view.on_key(key(KeyCode::Left));
        assert!(!view.drawers[0].expanded);
    }

    #[test]
    fn mouse_expands_graph_and_opens_commits_in_an_offset_body() {
        let fixture = Fixture::repo();
        commit_file(&fixture.0, "base", "mouse commit");
        let mut view = ScmView::new(&fixture.0);
        let mut terminal = Terminal::new(TestBackend::new(80, 32)).unwrap();
        let area = Rect::new(5, 3, 60, 25);
        terminal
            .draw(|frame| view.draw(frame, area, IconTheme::Emoji, None))
            .unwrap();
        let i = view
            .rows
            .iter()
            .position(|row| *row == Row::DrawerHeader(Drawer::Graph))
            .unwrap();
        let zone = view
            .zones
            .rows
            .iter()
            .find(|hit| hit.index == i)
            .unwrap()
            .area;
        assert_eq!(view.on_mouse(click(0, zone.y)), ScmAction::None);
        assert!(!view.drawers[0].expanded);
        view.on_mouse(click(zone.x + 2, zone.y));
        assert!(view.drawers[0].expanded);
        terminal
            .draw(|frame| view.draw(frame, area, IconTheme::Emoji, None))
            .unwrap();
        let i = view
            .rows
            .iter()
            .position(|row| *row == Row::DrawerLine(Drawer::Graph, 0))
            .unwrap();
        let zone = view
            .zones
            .rows
            .iter()
            .find(|hit| hit.index == i)
            .unwrap()
            .area;
        assert_eq!(
            view.on_mouse(click(zone.x + 3, zone.y)),
            ScmAction::OpenCommit(git(&fixture.0, &["rev-parse", "--short", "HEAD"]))
        );
    }

    #[test]
    fn keyboard_stages_unstages_and_commits_unicode_input_without_byte_indexing() {
        let fixture = Fixture::repo();
        std::fs::write(fixture.0.join("hello.rs"), "test").unwrap();
        let mut view = ScmView::new(&fixture.0);
        select_row(&mut view, Row::Unstaged(0, 0));
        view.on_key(key(KeyCode::Enter));
        assert_eq!(view.repos[0].status.staged.len(), 1);
        assert!(text(&draw(&mut view, None)).contains("Staged Changes"));
        select_row(&mut view, Row::Staged(0, 0));
        view.on_key(key(KeyCode::Enter));
        assert!(view.repos[0].status.staged.is_empty());
        view.on_key(key(KeyCode::Char('a')));
        view.on_key(key(KeyCode::Char('c')));
        for c in "hé界x".chars() {
            view.on_key(key(KeyCode::Char(c)));
        }
        view.on_key(key(KeyCode::Left));
        view.on_key(key(KeyCode::Backspace));
        assert_eq!(view.repos[0].message.iter().collect::<String>(), "héx");
        assert!(draw(&mut view, None).area.width > 0);
        view.on_key(key(KeyCode::Tab));
        assert_eq!(view.focus, Focus::Commit);
        view.on_key(key(KeyCode::Enter));
        assert_eq!(git(&fixture.0, &["log", "-1", "--format=%s"]), "héx");
        assert!(view.repos[0].message.is_empty());
        assert!(view.repos[0].status.staged.is_empty());
    }

    #[test]
    fn mouse_file_selection_opens_and_plus_stages() {
        let fixture = Fixture::repo();
        std::fs::write(fixture.0.join("hello.rs"), "test").unwrap();
        let mut view = ScmView::new(&fixture.0);
        draw(&mut view, None);
        let i = view
            .rows
            .iter()
            .position(|row| *row == Row::Unstaged(0, 0))
            .unwrap();
        let zone = view
            .zones
            .rows
            .iter()
            .find(|hit| hit.index == i)
            .unwrap()
            .area;
        assert!(matches!(
            view.on_mouse(click(zone.x + 4, zone.y)),
            ScmAction::OpenFile { staged: false, .. }
        ));
        let x = zone.x + file_action_start(zone.width as usize).unwrap() as u16 + 1;
        draw(&mut view, Some((x, zone.y)));
        view.on_mouse(click(x, zone.y));
        assert_eq!(view.repos[0].status.staged.len(), 1);
        assert!(view.repos[0].status.unstaged.is_empty());
    }

    #[test]
    fn errors_remain_visible_and_failed_commit_preserves_message() {
        let fixture = Fixture::repo();
        let mut view = ScmView::new(&fixture.0);
        select_row(&mut view, Row::DrawerHeader(Drawer::Graph));
        view.on_key(key(KeyCode::Enter));
        assert!(view.drawers[0].error); // unborn HEAD
        assert_eq!(view.drawers[0].refs, [DrawerRef::None]);
        let rendered = text(&draw(&mut view, None));
        assert!(rendered.contains("does not have any commits"));
        view.commit();
        assert!(text(&draw(&mut view, None)).contains("Enter a commit message"));
        view.repos[0].message = "keep me".chars().collect();
        view.repos[0].cursor = 7;
        view.commit();
        assert_eq!(view.repos[0].message.iter().collect::<String>(), "keep me");
        assert!(text(&draw(&mut view, None)).contains("No staged changes"));
        view.refresh();
        assert!(view.flash.is_none());
        assert_eq!(view.repos[0].cursor, 7);
        assert!(view.drawers[0].error);
    }

    #[test]
    fn empty_repo_message_and_all_drawers_are_non_actionable() {
        let fixture = Fixture::empty();
        let mut view = ScmView::new(&fixture.0);
        assert!(text(&draw(&mut view, None)).contains("No Git repository found"));
        assert!(view.active_root().is_none());
        for kind in Drawer::ALL {
            select_row(&mut view, Row::DrawerHeader(kind));
            view.on_key(key(KeyCode::Enter));
            assert!(view.drawers[drawer_index(kind)].error);
            select_row(&mut view, Row::DrawerLine(kind, 0));
            assert_eq!(view.on_key(key(KeyCode::Enter)), ScmAction::None);
        }
    }

    #[test]
    fn inline_commit_targets_clicked_repo_not_previous_file_selection() {
        let fixture = Fixture::repo();
        let child = fixture.0.join("child");
        init(&child);
        std::fs::write(fixture.0.join("parent.txt"), "parent").unwrap();
        std::fs::write(child.join("child.txt"), "child").unwrap();
        let mut view = ScmView::new(&fixture.0);
        assert_eq!(view.repos.len(), 2);
        select_row(&mut view, Row::Unstaged(0, 0));
        view.repos[1].message = "child only".chars().collect();
        view.repos[1].cursor = 10;
        view.repos[1].git.stage_all().unwrap();
        view.refresh();
        draw(&mut view, None);
        let i = view
            .rows
            .iter()
            .position(|row| *row == Row::Commit(1))
            .unwrap();
        let zone = view
            .zones
            .rows
            .iter()
            .find(|hit| hit.index == i)
            .unwrap()
            .area;
        view.on_mouse(click(zone.x + 5, zone.y + 1));
        assert_eq!(view.active_root(), Some(child.as_path()));
        assert_eq!(git(&child, &["log", "-1", "--format=%s"]), "child only");
        assert!(view.repos[1].message.is_empty());
    }

    #[test]
    fn refresh_reloads_only_expanded_drawers_and_preserves_folds() {
        let fixture = Fixture::repo();
        commit_file(&fixture.0, "base", "first");
        let mut view = ScmView::new(&fixture.0);
        select_row(&mut view, Row::DrawerHeader(Drawer::Graph));
        view.on_key(key(KeyCode::Enter));
        commit_file(&fixture.0, "next", "second");
        view.refresh();
        assert!(view.drawers[0].expanded);
        assert!(view.drawers[0].lines[0].contains("second"));
        assert!(view.drawers[1].lines.is_empty());
        assert_eq!(
            view.rows[view.selected.unwrap()],
            Row::DrawerHeader(Drawer::Graph)
        );
        select_row(&mut view, Row::DrawerHeader(Drawer::FileHistory));
        view.on_key(key(KeyCode::Enter));
        assert_eq!(view.drawers[2].lines, ["(select a file above)"]);
        assert_eq!(view.drawers[2].refs, [DrawerRef::None]);
        std::fs::write(fixture.0.join("base"), "modified").unwrap();
        view.refresh();
        select_row(&mut view, Row::Unstaged(0, 0));
        assert_eq!(view.history_target.as_deref(), Some("base"));
        assert!(view.drawers[2].lines[0].contains("first"));
        assert!(text(&draw(&mut view, None)).contains("File History  base"));
    }

    #[test]
    fn navigation_references_wrapping_and_small_terminals() {
        let fixture = Fixture::repo();
        let mut view = ScmView::new(&fixture.0);
        assert_eq!(view.on_key(key(KeyCode::Char('q'))), ScmAction::Quit);
        assert_eq!(
            view.on_key(key(KeyCode::Char('1'))),
            ScmAction::SwitchToExplorer
        );
        assert_eq!(
            view.on_key(key(KeyCode::Char('2'))),
            ScmAction::SwitchToSearch
        );
        view.on_key(key(KeyCode::Char('c')));
        assert_eq!(view.on_key(key(KeyCode::Char('q'))), ScmAction::None);
        assert_eq!(
            parse_drawer_ref(Drawer::Graph, "| * | abc1234 subject"),
            DrawerRef::Commit("abc1234".to_string())
        );
        assert_eq!(
            parse_drawer_ref(Drawer::Graph, "|\\ abc1234"),
            DrawerRef::None
        );
        assert_eq!(
            pretty_remote_line("origin git@github.com:owner/repo.git"),
            "origin  owner/repo"
        );
        assert_eq!(
            pretty_worktree_line("/home/me/project abc1234 [main]"),
            "project  ⎇ main"
        );
        let (rows, row, col) = wrap_message(&"é界abc".chars().collect::<Vec<_>>(), 5, 4);
        assert_eq!(rows, ["é界a", "bc"]);
        assert_eq!((row, col), (1, 2));
        for (width, height) in [(0, 0), (1, 1), (2, 4), (10, 8), (40, 20)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| view.draw(frame, frame.area(), IconTheme::Material, None))
                .unwrap();
        }
    }
}
