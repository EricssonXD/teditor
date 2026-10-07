mod editor;
mod icons;
mod scm;
mod scm_view;
mod search;
mod search_view;
mod tree;

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use editor::{EditAction, Editor, SaveOutcome};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph};
use scm::Git;
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
        crossterm::execute!(io::stdout(), EnableMouseCapture, EnableBracketedPaste)?;
        run(&mut terminal, &mut app)
    })();
    let _ = crossterm::execute!(io::stdout(), DisableMouseCapture, DisableBracketedPaste);
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
        Event::Paste(text) => {
            app.on_paste(&text);
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
    git_target: Option<EditTarget>,
    hunks: Vec<String>,
    hunk_ranges: Vec<(usize, usize)>,
    selected_hunk: usize,
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
            git_target: None,
            hunks: Vec::new(),
            hunk_ranges: Vec::new(),
            selected_hunk: 0,
        }
    }

    fn attach_git_diff(&mut self, target: EditTarget) {
        self.git_target = Some(target);
        self.hunks = split_diff_hunks(&self.lines);
        self.hunk_ranges = self
            .lines
            .iter()
            .enumerate()
            .filter_map(|(index, line)| line.starts_with("@@").then_some(index))
            .map(|start| {
                let end = self
                    .lines
                    .iter()
                    .enumerate()
                    .skip(start + 1)
                    .find(|(_, line)| line.starts_with("@@"))
                    .map_or(self.lines.len(), |(end, _)| end);
                (start, end)
            })
            .collect();
        self.selected_hunk = 0;
        self.anchor = self.hunk_ranges.first().map_or(0, |(start, _)| *start);
        self.scroll = self.anchor;
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ExplorerAction {
    Open,
    NewFile,
    NewFolder,
    Rename,
    Delete,
}

struct ContextMenu {
    path: PathBuf,
    is_dir: bool,
    actions: Vec<ExplorerAction>,
    selected: usize,
    origin: Position,
    area: Rect,
    items_area: Rect,
}

enum InputAction {
    CreateFile(PathBuf),
    CreateFolder(PathBuf),
    Rename(PathBuf),
    CreateBranch(PathBuf),
    CreateStash(PathBuf),
}

enum ConfirmAction {
    DeletePath(PathBuf),
    DiscardFile {
        root: PathBuf,
        path: String,
        staged: bool,
    },
    DeleteBranch {
        root: PathBuf,
        name: String,
    },
    DropStash {
        root: PathBuf,
        index: usize,
    },
}

enum Dialog {
    Input {
        title: String,
        value: Vec<char>,
        cursor: usize,
        action: InputAction,
    },
    Confirm {
        message: String,
        action: ConfirmAction,
    },
}

struct App {
    tree: Tree,
    view: View,
    icon_theme: icons::IconTheme,
    search: SearchView,
    scm: ScmView,
    status: Option<String>,
    context_menu: Option<ContextMenu>,
    dialog: Option<Dialog>,
    drag_source: Option<PathBuf>,
    drag_target: Option<PathBuf>,
    explorer_selected: usize,
    explorer_scroll: usize,
    body: Rect,
    activity_buttons: [Rect; 3],
    header_actions: [Rect; 2],
    mouse_pos: Option<(u16, u16)>,
    preview: Option<Preview>,
    editors: Vec<Editor>,
    active_editor: Option<usize>,
    sidebar_focused: bool,
    editor_tab_buttons: Vec<(Rect, usize)>,
    editor_tab_close_buttons: Vec<Rect>,
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
            context_menu: None,
            dialog: None,
            drag_source: None,
            drag_target: None,
            explorer_selected,
            explorer_scroll: 0,
            body: Rect::default(),
            activity_buttons: [Rect::default(); 3],
            header_actions: [Rect::default(); 2],
            mouse_pos: None,
            preview: None,
            editors: Vec::new(),
            active_editor: None,
            sidebar_focused: false,
            editor_tab_buttons: Vec::new(),
            editor_tab_close_buttons: Vec::new(),
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
        self.sidebar_focused = self.active_editor.is_some();
        self.status = None;
    }

    fn open_input(&mut self, title: &str, initial: &str, action: InputAction) {
        let value = initial.chars().collect::<Vec<_>>();
        self.dialog = Some(Dialog::Input {
            title: title.into(),
            cursor: value.len(),
            value,
            action,
        });
    }

    fn on_dialog_key(&mut self, key: KeyEvent) -> bool {
        let Some(dialog) = self.dialog.take() else {
            return false;
        };
        match dialog {
            Dialog::Input {
                title,
                mut value,
                mut cursor,
                action,
            } => {
                match key.code {
                    KeyCode::Esc => return false,
                    KeyCode::Enter => {
                        self.execute_input(action, &value.iter().collect::<String>());
                        return false;
                    }
                    KeyCode::Left => cursor = cursor.saturating_sub(1),
                    KeyCode::Right => cursor = (cursor + 1).min(value.len()),
                    KeyCode::Home => cursor = 0,
                    KeyCode::End => cursor = value.len(),
                    KeyCode::Backspace if cursor > 0 => {
                        cursor -= 1;
                        value.remove(cursor);
                    }
                    KeyCode::Delete if cursor < value.len() => {
                        value.remove(cursor);
                    }
                    KeyCode::Char(ch)
                        if !ch.is_control()
                            && !key.modifiers.intersects(
                                KeyModifiers::CONTROL | KeyModifiers::SUPER | KeyModifiers::ALT,
                            ) =>
                    {
                        value.insert(cursor, ch);
                        cursor += 1;
                    }
                    _ => {}
                }
                self.dialog = Some(Dialog::Input {
                    title,
                    value,
                    cursor,
                    action,
                });
            }
            Dialog::Confirm { message, action } => match key.code {
                KeyCode::Esc | KeyCode::Char('n' | 'N') => {}
                KeyCode::Char('y' | 'Y') | KeyCode::Enter => self.execute_confirm(action),
                _ => self.dialog = Some(Dialog::Confirm { message, action }),
            },
        }
        false
    }

    fn execute_input(&mut self, action: InputAction, value: &str) {
        let (result, verb) = match action {
            InputAction::CreateFile(parent) => (self.create_file(&parent, value), "Created"),
            InputAction::CreateFolder(parent) => (self.create_folder(&parent, value), "Created"),
            InputAction::Rename(path) => (self.rename_path(&path, value), "Renamed"),
            InputAction::CreateBranch(root) => {
                if self.dirty_editor_blocks_git(&root, None) {
                    return;
                }
                let result = Git::discover(&root).and_then(|git| {
                    git.create_branch(value)
                        .map(|_| format!("Created branch {value}"))
                });
                self.finish_worktree_action(&root, None, result, "Branch created");
                return;
            }
            InputAction::CreateStash(root) => {
                if self.dirty_editor_blocks_git(&root, None) {
                    return;
                }
                let result = Git::discover(&root).and_then(|git| git.stash_push(value));
                self.finish_worktree_action(&root, None, result, "Changes stashed");
                return;
            }
        };
        match result {
            Ok(Some(path)) => {
                self.refresh_tree_select(&path);
                if path.is_file() {
                    self.open_editor(&path, 0);
                }
                self.status = Some(format!("{verb} {}", path.display()));
            }
            Ok(None) => self.status = Some(format!("Renamed to {value}")),
            Err(error) => self.status = Some(error),
        }
    }

    fn safe_child(&self, parent: &Path, name: &str) -> Result<PathBuf, String> {
        let name_path = Path::new(name);
        if name.trim().is_empty()
            || name_path.is_absolute()
            || name_path.components().count() != 1
            || !matches!(
                name_path.components().next(),
                Some(std::path::Component::Normal(_))
            )
        {
            return Err("Use a single file or folder name".into());
        }
        let root =
            std::fs::canonicalize(self.tree.root_path()).map_err(|error| error.to_string())?;
        let parent = std::fs::canonicalize(parent).map_err(|error| error.to_string())?;
        if !parent.starts_with(&root) {
            return Err("File operation would leave the workspace".into());
        }
        Ok(parent.join(name_path))
    }

    fn create_file(&mut self, parent: &Path, name: &str) -> Result<Option<PathBuf>, String> {
        let path = self.safe_child(parent, name)?;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|error| format!("Cannot create file: {error}"))?;
        Ok(Some(path))
    }

    fn create_folder(&mut self, parent: &Path, name: &str) -> Result<Option<PathBuf>, String> {
        let path = self.safe_child(parent, name)?;
        std::fs::create_dir(&path).map_err(|error| format!("Cannot create folder: {error}"))?;
        Ok(Some(path))
    }

    fn rename_path(&mut self, path: &Path, name: &str) -> Result<Option<PathBuf>, String> {
        if path == self.tree.root_path() {
            return Err("Cannot rename the workspace root".into());
        }
        let parent = path.parent().ok_or("File has no parent directory")?;
        let destination = self.safe_child(parent, name)?;
        if destination.exists() {
            return Err("A file or folder with that name already exists".into());
        }
        std::fs::rename(path, &destination).map_err(|error| format!("Cannot rename: {error}"))?;
        for editor in &mut self.editors {
            if editor.path() == path {
                editor.rename_path(destination.clone());
            }
        }
        Ok(Some(destination))
    }

    fn move_path_into(&mut self, source: &Path, target: &Path) {
        if source == self.tree.root_path() || target == source || target.starts_with(source) {
            self.status = Some("Cannot move a folder into itself".into());
            self.drag_source = None;
            return;
        }
        let Some(name) = source.file_name() else {
            self.status = Some("Cannot move the workspace root".into());
            return;
        };
        let destination = match self.safe_child(target, &name.to_string_lossy()) {
            Ok(destination) => destination,
            Err(error) => {
                self.status = Some(error);
                return;
            }
        };
        if destination.exists() {
            self.status = Some("Destination already exists".into());
            return;
        }
        match std::fs::rename(source, &destination) {
            Ok(()) => {
                for editor in &mut self.editors {
                    let old = editor.path().to_path_buf();
                    if (old == source || old.starts_with(source))
                        && let Ok(relative) = old.strip_prefix(source)
                    {
                        editor.rename_path(destination.join(relative));
                    }
                }
                self.preview = None;
                self.refresh_tree_select(&destination);
                self.status = Some(format!("Moved to {}", destination.display()));
            }
            Err(error) => self.status = Some(format!("Cannot move: {error}")),
        }
    }

    fn refresh_tree_select(&mut self, path: &Path) {
        let root = self.tree.root_path();
        if let Some(parent) = path.parent() {
            let mut ancestors = parent.ancestors().collect::<Vec<_>>();
            ancestors.reverse();
            for ancestor in ancestors
                .into_iter()
                .filter(|ancestor| ancestor.starts_with(&root))
            {
                if ancestor != root && !self.tree.is_expanded(ancestor) {
                    self.tree.toggle(ancestor);
                }
            }
        }
        self.tree.refresh();
        let rows = self.tree.rows();
        if let Some(index) = rows.iter().position(|row| row.path == path) {
            self.explorer_selected = index;
        }
    }

    fn execute_confirm(&mut self, action: ConfirmAction) {
        match action {
            ConfirmAction::DeletePath(path) => {
                if self
                    .editors
                    .iter()
                    .any(|editor| editor.path().starts_with(&path) && editor.dirty())
                {
                    self.status = Some(if path.is_dir() {
                        "Save or discard open files before deleting this folder".into()
                    } else {
                        "Save or discard the open file before deleting it".into()
                    });
                    return;
                }
                let root = self.tree.root_path();
                let Some(parent) = path.parent() else {
                    self.status = Some("Cannot delete the workspace root".into());
                    return;
                };
                let confined = std::fs::canonicalize(parent)
                    .ok()
                    .zip(std::fs::canonicalize(&root).ok())
                    .is_some_and(|(parent, root)| parent.starts_with(root));
                if !confined || path == root {
                    self.status = Some("Delete target is outside the workspace".into());
                    return;
                }
                let result = if path.is_dir() {
                    std::fs::remove_dir_all(&path)
                } else {
                    std::fs::remove_file(&path)
                };
                match result {
                    Ok(()) => {
                        let mut index = 0;
                        while index < self.editors.len() {
                            if self.editors[index].path().starts_with(&path) {
                                self.close_editor_at(index);
                            } else {
                                index += 1;
                            }
                        }
                        self.tree.refresh();
                        self.explorer_selected = self
                            .explorer_selected
                            .min(self.tree.rows().len().saturating_sub(1));
                        let message = format!("Deleted {}", path.display());
                        self.status = Some(message.clone());
                        self.scm.refresh();
                        self.scm.show_feedback(message, false);
                    }
                    Err(error) => self.status = Some(format!("Cannot delete: {error}")),
                }
            }
            ConfirmAction::DiscardFile { root, path, staged } => {
                if self.dirty_editor_blocks_git(&root, Some(Path::new(&path))) {
                    return;
                }
                let result = Git::discover(&root).and_then(|git| {
                    git.discard_path(&path, staged)
                        .map(|_| format!("Discarded {path}"))
                });
                self.finish_worktree_action(
                    &root,
                    Some(Path::new(&path)),
                    result,
                    "Changes discarded",
                );
            }
            ConfirmAction::DeleteBranch { root, name } => {
                let result = Git::discover(&root).and_then(|git| {
                    git.delete_branch(&name)
                        .map(|_| format!("Deleted branch {name}"))
                });
                self.finish_git_action(result, "Branch deleted");
            }
            ConfirmAction::DropStash { root, index } => {
                let result = Git::discover(&root).and_then(|git| git.stash_drop(index));
                self.finish_git_action(result, "Stash dropped");
            }
        }
    }

    fn on_context_menu_key(&mut self, key: KeyEvent) {
        let Some(mut menu) = self.context_menu.take() else {
            return;
        };
        match key.code {
            KeyCode::Esc => {}
            KeyCode::Up | KeyCode::Char('k') => {
                menu.selected = menu.selected.saturating_sub(1);
                self.context_menu = Some(menu);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                menu.selected = (menu.selected + 1).min(menu.actions.len().saturating_sub(1));
                self.context_menu = Some(menu);
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                self.activate_context_action(menu.path, menu.is_dir, menu.actions[menu.selected]);
            }
            _ => self.context_menu = Some(menu),
        }
    }

    fn activate_context_action(&mut self, path: PathBuf, is_dir: bool, action: ExplorerAction) {
        let parent = if is_dir {
            path.clone()
        } else {
            path.parent().unwrap_or(&path).to_path_buf()
        };
        match action {
            ExplorerAction::Open if is_dir => {
                self.tree.toggle(&path);
                self.tree.refresh();
            }
            ExplorerAction::Open => self.open_editor(&path, 0),
            ExplorerAction::NewFile => {
                self.open_input("New File", "", InputAction::CreateFile(parent))
            }
            ExplorerAction::NewFolder => {
                self.open_input("New Folder", "", InputAction::CreateFolder(parent))
            }
            ExplorerAction::Rename => {
                let name = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or_default()
                    .to_owned();
                self.open_input("Rename", &name, InputAction::Rename(path));
            }
            ExplorerAction::Delete => {
                self.dialog = Some(Dialog::Confirm {
                    message: format!("Delete {}? [y/N]", path.display()),
                    action: ConfirmAction::DeletePath(path),
                });
            }
        }
    }

    fn on_key(&mut self, key: KeyEvent) -> bool {
        if key.kind != KeyEventKind::Press {
            return false;
        }
        if self.dialog.is_some() {
            return self.on_dialog_key(key);
        }
        if self.context_menu.is_some() {
            self.on_context_menu_key(key);
            return false;
        }
        let shortcut = (key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT))
            || key.modifiers.contains(KeyModifiers::SUPER);
        if self.active_editor.is_some() {
            if key.code == KeyCode::F(6) {
                self.sidebar_focused = !self.sidebar_focused;
                return false;
            }
            if shortcut && matches!(key.code, KeyCode::Char('f' | 'F')) {
                self.switch_view(View::Search, true);
                return false;
            }
            if shortcut && matches!(key.code, KeyCode::Char('1' | '2' | '3')) {
                self.switch_view(view_for_key(key.code), false);
                return false;
            }
            if self.sidebar_focused {
                return self.on_sidebar_key(key);
            }
            return self.on_editor_key(key);
        }
        if self.preview.is_some() {
            return self.on_preview_key(key);
        }

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

    fn on_sidebar_key(&mut self, key: KeyEvent) -> bool {
        match self.view {
            View::Explorer => self.on_explorer_key(key),
            View::Search => {
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
            ScmAction::CreateBranch(root) => {
                self.open_input("Create Branch", "", InputAction::CreateBranch(root));
                false
            }
            ScmAction::CreateStash(root) => {
                self.open_input("Stash Changes", "WIP", InputAction::CreateStash(root));
                false
            }
            ScmAction::SwitchBranch { root, name } => {
                if self.dirty_editor_blocks_git(&root, None) {
                    return false;
                }
                let result = Git::discover(&root).and_then(|git| {
                    git.switch_branch(&name)
                        .map(|_| format!("Switched to {name}"))
                });
                self.finish_worktree_action(&root, None, result, "Branch switched");
                false
            }
            ScmAction::CheckoutTag { root, name } => {
                if self.dirty_editor_blocks_git(&root, None) {
                    return false;
                }
                let result = Git::discover(&root).and_then(|git| {
                    git.checkout_tag(&name)
                        .map(|_| format!("Checked out tag {name}"))
                });
                self.finish_worktree_action(&root, None, result, "Tag checked out");
                false
            }
            ScmAction::CheckoutCommit { root, hash } => {
                if self.dirty_editor_blocks_git(&root, None) {
                    return false;
                }
                let result = Git::discover(&root).and_then(|git| {
                    git.checkout_commit(&hash)
                        .map(|_| format!("Checked out {hash}"))
                });
                self.finish_worktree_action(&root, None, result, "Commit checked out");
                false
            }
            ScmAction::Pull(root) => {
                if self.dirty_editor_blocks_git(&root, None) {
                    return false;
                }
                let result = Git::discover(&root).and_then(|git| git.pull());
                self.finish_worktree_action(&root, None, result, "Pulled changes");
                false
            }
            ScmAction::DeleteBranch { root, name } => {
                self.dialog = Some(Dialog::Confirm {
                    message: format!("Delete branch {name}? [y/N]"),
                    action: ConfirmAction::DeleteBranch { root, name },
                });
                false
            }
            ScmAction::DiscardFile { root, path, staged } => {
                self.dialog = Some(Dialog::Confirm {
                    message: if staged {
                        format!("Discard all staged and working changes to {path}? [y/N]")
                    } else {
                        format!("Discard working changes to {path}? [y/N]")
                    },
                    action: ConfirmAction::DiscardFile { root, path, staged },
                });
                false
            }
            ScmAction::ApplyStash { root, index } => {
                if self.dirty_editor_blocks_git(&root, None) {
                    return false;
                }
                let result = Git::discover(&root).and_then(|git| git.stash_apply(index));
                self.finish_worktree_action(&root, None, result, "Stash applied");
                false
            }
            ScmAction::DropStash { root, index } => {
                self.dialog = Some(Dialog::Confirm {
                    message: format!("Drop stash@{{{index}}}? [y/N]"),
                    action: ConfirmAction::DropStash { root, index },
                });
                false
            }
        }
    }

    fn finish_git_action(&mut self, result: Result<String, String>, fallback: &str) {
        self.scm.refresh();
        let (message, error) = match result {
            Ok(output) if !output.trim().is_empty() => (output.trim().to_string(), false),
            Ok(_) => (fallback.to_string(), false),
            Err(error) => (error, true),
        };
        self.scm.show_feedback(message.clone(), error);
        self.status = Some(message);
    }

    fn dirty_editor_blocks_git(&mut self, root: &Path, affected: Option<&Path>) -> bool {
        let affected = affected.map(|path| {
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                root.join(path)
            }
        });
        let blocked = self.editors.iter().any(|editor| {
            editor.dirty()
                && editor.path().starts_with(root)
                && affected
                    .as_ref()
                    .is_none_or(|path| editor.path() == path || editor.path().starts_with(path))
        });
        if blocked {
            let message = if affected.is_some() {
                "Save or discard the open editor before changing this path"
            } else {
                "Save or discard open editor tabs before changing the repository worktree"
            };
            self.status = Some(message.into());
            self.scm.show_feedback(message.into(), true);
        }
        blocked
    }

    fn finish_worktree_action(
        &mut self,
        root: &Path,
        affected: Option<&Path>,
        result: Result<String, String>,
        fallback: &str,
    ) {
        let affected = affected.map(|path| {
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                root.join(path)
            }
        });
        let mut index = 0;
        while index < self.editors.len() {
            let path = self.editors[index].path().to_path_buf();
            let touches = path.starts_with(root)
                && affected
                    .as_ref()
                    .is_none_or(|target| path == *target || path.starts_with(target));
            if touches
                && !self.editors[index].dirty()
                && (!path.is_file()
                    || self.editors[index]
                        .reload_if_changed(MAX_PREVIEW_BYTES)
                        .is_err())
            {
                self.close_editor_at(index);
            } else {
                index += 1;
            }
        }
        self.tree.refresh();
        self.explorer_selected = self
            .explorer_selected
            .min(self.tree.rows().len().saturating_sub(1));
        self.finish_git_action(result, fallback);
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
                    let title = format!(
                        "{} · {name}",
                        if staged { "Staged Changes" } else { "Changes" }
                    );
                    let target = EditTarget {
                        path: path.clone(),
                        root: root.to_path_buf(),
                        relative: relative.to_path_buf(),
                        staged,
                    };
                    let mut preview = Preview::new(title, diff, false, 0);
                    preview.attach_git_diff(target);
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
        let target = EditTarget {
            path: path.clone(),
            root: root.to_path_buf(),
            relative: relative.to_path_buf(),
            staged: staged.unwrap_or(false),
        };
        preview.edit_target = staged.is_none().then(|| target.clone());
        preview.git_target = staged.map(|_| target);
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
        if let Some(index) = self.editors.iter().position(|editor| editor.path() == path) {
            self.active_editor = Some(index);
            self.sidebar_focused = false;
            self.editors[index].place_cursor(line, 0);
            self.status = None;
            return;
        }
        match Editor::open(path, MAX_PREVIEW_BYTES) {
            Ok(mut editor) => {
                editor.place_cursor(line, 0);
                self.editors.push(editor);
                self.active_editor = Some(self.editors.len() - 1);
                self.sidebar_focused = false;
                self.status = None;
            }
            Err(error) => {
                self.status = Some(format!("Cannot edit {}: {error}", path.display()));
            }
        }
    }

    fn active_editor(&self) -> Option<&Editor> {
        self.active_editor.and_then(|index| self.editors.get(index))
    }

    fn active_editor_mut(&mut self) -> Option<&mut Editor> {
        self.active_editor
            .and_then(|index| self.editors.get_mut(index))
    }

    fn close_active_editor(&mut self) {
        if let Some(index) = self.active_editor {
            self.close_editor_at(index);
        }
    }

    fn close_editor_at(&mut self, index: usize) {
        if index >= self.editors.len() {
            return;
        }
        self.editors.remove(index);
        self.active_editor = if self.editors.is_empty() {
            None
        } else {
            let active = self.active_editor.unwrap_or(0);
            Some(if active > index {
                active - 1
            } else {
                active.min(self.editors.len() - 1)
            })
        };
    }

    fn on_paste(&mut self, text: &str) {
        if let Some(editor) = self.active_editor_mut() {
            editor.paste(text);
            self.status = None;
        }
    }

    fn on_editor_key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL)
            || key.modifiers.contains(KeyModifiers::SUPER);
        let shortcut = ctrl && !key.modifiers.contains(KeyModifiers::ALT);
        if shortcut && matches!(key.code, KeyCode::Char('c' | 'C' | 'x' | 'X')) {
            let cutting = matches!(key.code, KeyCode::Char('x' | 'X'));
            let text = self.active_editor().and_then(Editor::selected_text);
            match text {
                Some(text) => match copy_to_clipboard(&text) {
                    Ok(()) => {
                        if cutting {
                            self.active_editor_mut().and_then(Editor::cut_selection);
                        }
                        self.status = None;
                    }
                    Err(error) => self.status = Some(format!("Clipboard copy failed: {error}")),
                },
                None => self.status = Some("Select text first".into()),
            }
            return false;
        }
        if shortcut && matches!(key.code, KeyCode::Char('w' | 'W')) {
            if self.active_editor().is_some_and(Editor::dirty) {
                self.status = Some("Unsaved changes · Ctrl+S save / Ctrl+Q discard".into());
            } else {
                self.close_active_editor();
                self.status = None;
            }
            return false;
        }
        let page = usize::from(self.editor_body.height).max(1);
        let action = self
            .active_editor_mut()
            .map_or(EditAction::None, |editor| editor.handle_key(key, page));
        match action {
            EditAction::None => {
                if self.active_editor().is_some_and(Editor::dirty) {
                    self.status = None;
                }
            }
            EditAction::Close => {
                if self.active_editor().is_some_and(Editor::dirty) {
                    self.status = Some("Unsaved changes · Ctrl+S save / Ctrl+Q discard".into());
                } else {
                    self.close_active_editor();
                    self.status = None;
                }
            }
            EditAction::Discard => {
                self.close_active_editor();
                self.status = None;
            }
            EditAction::Save => {
                let force = self.active_editor().is_some_and(Editor::external_changed);
                match self
                    .active_editor_mut()
                    .expect("active editor exists")
                    .save(force)
                {
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
                if self.active_editor().is_some_and(Editor::dirty) {
                    self.status = Some("Unsaved changes · save or Ctrl+Q before reload".into());
                } else {
                    match self
                        .active_editor_mut()
                        .expect("active editor exists")
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
                .active_editor()
                .is_some_and(|editor| editor.path() == target.path)
        {
            self.open_file_preview(&target.root, &target.relative, Some(target.staged), 0);
        }
    }

    fn on_preview_key(&mut self, key: KeyEvent) -> bool {
        if let Some(target) = self
            .preview
            .as_ref()
            .and_then(|preview| preview.git_target.clone())
        {
            let has_hunks = self
                .preview
                .as_ref()
                .is_some_and(|preview| !preview.hunks.is_empty());
            match key.code {
                KeyCode::Char('[' | ']') => {
                    if let Some(preview) = &mut self.preview
                        && !preview.hunk_ranges.is_empty()
                    {
                        if key.code == KeyCode::Char('[') {
                            preview.selected_hunk = preview.selected_hunk.saturating_sub(1);
                        } else {
                            preview.selected_hunk =
                                (preview.selected_hunk + 1).min(preview.hunk_ranges.len() - 1);
                        }
                        preview.anchor = preview.hunk_ranges[preview.selected_hunk].0;
                        preview.scroll = preview.anchor;
                    }
                    return false;
                }
                KeyCode::Char('s') if !target.staged && has_hunks => {
                    self.apply_preview_hunk();
                    return false;
                }
                KeyCode::Char('u') if target.staged && has_hunks => {
                    self.apply_preview_hunk();
                    return false;
                }
                KeyCode::Char('d') => {
                    self.dialog = Some(Dialog::Confirm {
                        message: if target.staged {
                            format!(
                                "Discard all staged and working changes to {}? [y/N]",
                                target.relative.display()
                            )
                        } else {
                            format!(
                                "Discard working changes to {}? [y/N]",
                                target.relative.display()
                            )
                        },
                        action: ConfirmAction::DiscardFile {
                            root: target.root,
                            path: target.relative.to_string_lossy().into_owned(),
                            staged: target.staged,
                        },
                    });
                    return false;
                }
                _ => {}
            }
        }
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

    fn apply_preview_hunk(&mut self) {
        let Some((target, patch)) = self.preview.as_ref().and_then(|preview| {
            Some((
                preview.git_target.clone()?,
                preview.hunks.get(preview.selected_hunk)?.clone(),
            ))
        }) else {
            return;
        };
        let result = Git::discover(&target.root).and_then(|git| {
            git.apply_hunk(&patch, target.staged).map(|_| {
                if target.staged {
                    "Hunk unstaged"
                } else {
                    "Hunk staged"
                }
                .to_string()
            })
        });
        match result {
            Ok(message) => {
                self.scm.refresh();
                self.scm.show_feedback(message, false);
                self.open_file_preview(&target.root, &target.relative, Some(target.staged), 0);
            }
            Err(error) => {
                self.scm.show_feedback(error.clone(), true);
                self.status = Some(error);
            }
        }
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
            KeyCode::Char('n') | KeyCode::Char('N') => {
                if let Some(row) = rows.get(self.explorer_selected) {
                    let parent = if row.is_dir {
                        row.path.clone()
                    } else {
                        row.path.parent().unwrap_or(Path::new(".")).to_path_buf()
                    };
                    let action = if key.code == KeyCode::Char('N') {
                        InputAction::CreateFolder(parent)
                    } else {
                        InputAction::CreateFile(parent)
                    };
                    self.open_input(
                        if key.code == KeyCode::Char('N') {
                            "New Folder"
                        } else {
                            "New File"
                        },
                        "",
                        action,
                    );
                }
            }
            KeyCode::F(2) => {
                if let Some(row) = rows.get(self.explorer_selected)
                    && row.path != self.tree.root_path()
                {
                    let name = row
                        .path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or_default();
                    self.open_input("Rename", name, InputAction::Rename(row.path.clone()));
                }
            }
            KeyCode::Delete => {
                if let Some(row) = rows.get(self.explorer_selected)
                    && row.path != self.tree.root_path()
                {
                    self.dialog = Some(Dialog::Confirm {
                        message: format!("Delete {}? [y/N]", row.path.display()),
                        action: ConfirmAction::DeletePath(row.path.clone()),
                    });
                }
            }
            KeyCode::Char('m') => {
                if let Some(row) = rows.get(self.explorer_selected) {
                    let y = self.body.y.saturating_add(
                        self.explorer_selected.saturating_sub(self.explorer_scroll) as u16,
                    );
                    self.open_context_menu(
                        row.path.clone(),
                        row.is_dir,
                        Position::new(self.body.x.saturating_add(4), y),
                    );
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

    fn open_context_menu(&mut self, path: PathBuf, is_dir: bool, origin: Position) {
        let mut actions = vec![
            ExplorerAction::Open,
            ExplorerAction::NewFile,
            ExplorerAction::NewFolder,
        ];
        if path != self.tree.root_path() {
            actions.extend([ExplorerAction::Rename, ExplorerAction::Delete]);
        }
        self.context_menu = Some(ContextMenu {
            path,
            is_dir,
            actions,
            selected: 0,
            origin,
            area: Rect::default(),
            items_area: Rect::default(),
        });
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
        let position = Position::new(mouse.column, mouse.row);
        if self.dialog.is_some() {
            return;
        }
        if self.context_menu.is_some() {
            let Some(menu) = self.context_menu.take() else {
                return;
            };
            if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                self.context_menu = Some(menu);
                return;
            }
            if menu.items_area.contains(position) {
                let index = usize::from(mouse.row.saturating_sub(menu.items_area.y));
                if let Some(action) = menu.actions.get(index).copied() {
                    self.activate_context_action(menu.path, menu.is_dir, action);
                }
            }
            return;
        }
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            for (index, button) in self.activity_buttons.iter().enumerate() {
                if button.contains(position) {
                    self.switch_view(
                        [View::Explorer, View::Search, View::SourceControl][index],
                        false,
                    );
                    return;
                }
            }
        }
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && let Some((_, index)) = self
                .editor_tab_buttons
                .iter()
                .find(|(area, _)| area.contains(position))
        {
            let index = *index;
            if self
                .editor_tab_close_buttons
                .get(index)
                .is_some_and(|area| area.contains(position))
            {
                if self.editors[index].dirty() {
                    self.status = Some("Unsaved changes · Ctrl+S save / Ctrl+Q discard".into());
                } else {
                    self.close_editor_at(index);
                    self.status = None;
                }
            } else {
                self.active_editor = Some(index);
                self.sidebar_focused = false;
                self.status = None;
            }
            return;
        }
        if self.active_editor.is_some() {
            if self.editor_body.contains(position) {
                self.sidebar_focused = false;
                let editor_body = self.editor_body;
                if let Some(editor) = self.active_editor_mut() {
                    editor.on_mouse(mouse, editor_body);
                }
                return;
            }
            self.dispatch_sidebar_mouse(mouse);
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
        self.dispatch_sidebar_mouse(mouse);
    }

    fn dispatch_sidebar_mouse(&mut self, mouse: MouseEvent) {
        if self.active_editor.is_some() {
            self.sidebar_focused = true;
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
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Right))
            && self.body.contains(position)
        {
            let index = self.body_index(mouse.row);
            if let Some(row) = self.tree.rows().get(index) {
                self.explorer_selected = index;
                self.open_context_menu(
                    row.path.clone(),
                    row.is_dir,
                    Position::new(mouse.column, mouse.row),
                );
            }
            return;
        }
        if matches!(mouse.kind, MouseEventKind::Up(MouseButton::Left)) {
            if let Some(source) = self.drag_source.take()
                && self.body.contains(position)
            {
                let index = self.body_index(mouse.row);
                if let Some(target) = self.tree.rows().get(index).filter(|row| row.is_dir) {
                    self.move_path_into(&source, &target.path);
                }
            }
            self.drag_target = None;
            return;
        }
        if matches!(mouse.kind, MouseEventKind::Drag(MouseButton::Left)) {
            if self.drag_source.is_some() && self.body.contains(position) {
                let index = self.body_index(mouse.row);
                self.drag_target = self
                    .tree
                    .rows()
                    .get(index)
                    .filter(|row| row.is_dir)
                    .map(|row| row.path.clone());
            }
            return;
        }
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
            } else {
                self.drag_source = Some(row.path.clone());
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
        if self.preview.is_none()
            && self.active_editor.is_none()
            && self.view == View::SourceControl
        {
            self.scm
                .draw(frame, remaining, self.icon_theme, self.mouse_pos);
            self.draw_overlays(frame, area);
            return;
        }
        let [panel, footer] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(remaining);
        if self.active_editor.is_some() {
            let [sidebar, editor_area] =
                Layout::horizontal([Constraint::Percentage(32), Constraint::Min(0)]).areas(panel);
            match self.view {
                View::Explorer => self.draw_explorer_panel(frame, sidebar),
                View::Search => self
                    .search
                    .draw(frame, sidebar, self.icon_theme, self.mouse_pos),
                View::SourceControl => {
                    self.scm
                        .draw(frame, sidebar, self.icon_theme, self.mouse_pos)
                }
            }
            self.draw_editor(frame, editor_area);
        } else if self.preview.is_some() {
            self.draw_preview(frame, panel);
        } else {
            match self.view {
                View::Explorer => self.draw_explorer_panel(frame, panel),
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
        self.draw_overlays(frame, area);
    }

    fn draw_overlays(&mut self, frame: &mut Frame, area: Rect) {
        if let Some(menu) = &mut self.context_menu {
            let labels = menu
                .actions
                .iter()
                .map(|action| match action {
                    ExplorerAction::Open if menu.is_dir => "Open / Collapse",
                    ExplorerAction::Open => "Open",
                    ExplorerAction::NewFile => "New File",
                    ExplorerAction::NewFolder => "New Folder",
                    ExplorerAction::Rename => "Rename",
                    ExplorerAction::Delete => "Delete",
                })
                .collect::<Vec<_>>();
            let width = labels
                .iter()
                .map(|label| Span::raw(*label).width())
                .max()
                .unwrap_or(8)
                .saturating_add(4)
                .min(usize::from(area.width)) as u16;
            let height = (labels.len() as u16 + 2).min(area.height);
            let popup = Rect::new(
                menu.origin.x.min(area.right().saturating_sub(width)),
                menu.origin.y.min(area.bottom().saturating_sub(height)),
                width,
                height,
            );
            let block = Block::bordered()
                .border_style(Style::default().fg(ACCENT))
                .style(Style::default().bg(BG));
            let items_area = block.inner(popup);
            let items = labels
                .iter()
                .enumerate()
                .map(|(index, label)| {
                    ListItem::new(*label).style(if index == menu.selected {
                        Style::default().fg(FG).bg(SELECTED)
                    } else {
                        Style::default().fg(FG).bg(BG)
                    })
                })
                .collect::<Vec<_>>();
            menu.area = popup;
            menu.items_area = items_area;
            frame.render_widget(Clear, popup);
            frame.render_widget(block, popup);
            frame.render_widget(List::new(items).style(Style::default().bg(BG)), items_area);
        }
        if let Some(dialog) = &self.dialog {
            let (title, content) = match dialog {
                Dialog::Input {
                    title,
                    value,
                    cursor,
                    ..
                } => {
                    let left = value[..*cursor].iter().collect::<String>();
                    let right = value[*cursor..].iter().collect::<String>();
                    (
                        title.as_str(),
                        format!("{left}│{right}\nEnter confirm · Esc cancel"),
                    )
                }
                Dialog::Confirm { message, .. } => (
                    "Confirm",
                    format!("{message}\nEnter / y confirm · Esc / n cancel"),
                ),
            };
            let width = (Span::raw(content.lines().next().unwrap_or_default()).width() as u16 + 4)
                .max(24)
                .min(area.width);
            let height = 5.min(area.height);
            let popup = Rect::new(
                area.x + area.width.saturating_sub(width) / 2,
                area.y + area.height.saturating_sub(height) / 2,
                width,
                height,
            );
            let block = Block::bordered()
                .title(title)
                .border_style(Style::default().fg(ACCENT))
                .style(Style::default().bg(BG));
            let inner = block.inner(popup);
            frame.render_widget(Clear, popup);
            frame.render_widget(block, popup);
            frame.render_widget(Paragraph::new(content), inner);
        }
    }

    fn draw_activity(&mut self, frame: &mut Frame, area: Rect) {
        let slots = Layout::horizontal([Constraint::Length(16); 3]).split(area);
        let glyphs = match self.icon_theme {
            icons::IconTheme::Material => ["\u{f07b}", "\u{f002}", "\u{f126}"],
            icons::IconTheme::Emoji => ["📁", "🔍", "🔀"],
        };
        let labels = ["Explorer", "Search", "Source Control"];
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
                Paragraph::new(Line::from(vec![
                    Span::raw(format!("{} ", glyphs[index])),
                    Span::raw(labels[index]),
                ]))
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

    fn draw_explorer_panel(&mut self, frame: &mut Frame, area: Rect) {
        let [header, body] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
        self.draw_explorer_header(frame, header);
        self.body = body;
        self.draw_explorer(frame, body);
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
        let decorations = self.scm.decorations(&self.tree.root_path());
        let items = rows
            .iter()
            .enumerate()
            .skip(self.explorer_scroll)
            .take(height)
            .map(|(index, row)| {
                row_item(
                    row,
                    self.icon_theme,
                    index == self.explorer_selected,
                    self.drag_target.as_deref() == Some(row.path.as_path()),
                    decorations.get(&row.path).copied(),
                )
            })
            .collect::<Vec<_>>();
        frame.render_widget(List::new(items).style(Style::default().bg(BG)), body);
    }

    fn draw_preview(&mut self, frame: &mut Frame, area: Rect) {
        let Some(preview) = &mut self.preview else {
            return;
        };
        let [header, body] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
        let title = if preview.hunk_ranges.is_empty() {
            preview.title.clone()
        } else {
            format!(
                "{} · Hunk {}/{}",
                preview.title,
                preview.selected_hunk + 1,
                preview.hunk_ranges.len(),
            )
        };
        frame.render_widget(
            Paragraph::new(terminal_safe(&title))
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
                let selected_hunk = preview
                    .hunk_ranges
                    .get(preview.selected_hunk)
                    .is_some_and(|(start, end)| (*start..*end).contains(&index));
                let row_bg = if selected_hunk || (preview.line_numbers && index == preview.anchor) {
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
        self.editor_tab_buttons.clear();
        self.editor_tab_close_buttons = vec![Rect::default(); self.editors.len()];
        let [tabs, body] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
        let mut x = tabs.x;
        for (index, editor) in self.editors.iter().enumerate() {
            if x >= tabs.right() {
                break;
            }
            let name = editor
                .path()
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("file");
            let label = format!(" {}{} × ", name, if editor.dirty() { " •" } else { "" });
            let width = (Span::raw(&label).width() as u16).min(tabs.right() - x);
            let tab = Rect::new(x, tabs.y, width, 1);
            self.editor_tab_buttons.push((tab, index));
            let close = Rect::new(tab.right().saturating_sub(2), tab.y, 2.min(tab.width), 1);
            self.editor_tab_close_buttons[index] = close;
            let active = self.active_editor == Some(index);
            frame.render_widget(
                Paragraph::new(terminal_safe(&label)).style(if active {
                    Style::default()
                        .fg(FG)
                        .bg(SELECTED)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(MUTED).bg(BG)
                }),
                tab,
            );
            x = tab.right();
        }
        let Some(index) = self.active_editor else {
            self.editor_body = body;
            return;
        };
        let Some(editor) = self.editors.get_mut(index) else {
            return;
        };
        editor.ensure_visible(usize::from(body.width), usize::from(body.height));
        self.editor_body = body;
        let rows = editor
            .lines()
            .iter()
            .enumerate()
            .skip(editor.scroll())
            .take(usize::from(body.height))
            .map(|(index, line)| {
                let current = index == editor.cursor_line();
                let selection = editor.selection().and_then(|(start, end)| {
                    (index >= start.line && index <= end.line).then_some((
                        if index == start.line { start.col } else { 0 },
                        if index == end.line {
                            end.col
                        } else {
                            line.len()
                        },
                    ))
                });
                let mut spans = vec![Span::styled(
                    format!("{:>5} ", index + 1),
                    Style::default().fg(if current { ACCENT } else { MUTED }),
                )];
                spans.extend(editor_line_spans(
                    line,
                    current.then_some(editor.cursor_col()),
                    selection,
                    editor.horizontal_scroll(),
                ));
                ListItem::new(Line::from(spans))
            })
            .collect::<Vec<_>>();
        frame.render_widget(List::new(rows).style(Style::default().bg(BG)), body);
    }

    fn footer(&self) -> String {
        if let Some(editor) = self.active_editor() {
            let language = language_for_path(editor.path());
            let encoding = if editor.has_bom() {
                "UTF-8 BOM"
            } else {
                "UTF-8"
            };
            let details = format!(
                "{language} · Ln {}/{}, Col {} · {encoding} · {}",
                editor.cursor_line() + 1,
                editor.line_count(),
                editor.cursor_col() + 1,
                editor.line_ending(),
            );
            return if let Some(status) = &self.status {
                format!(
                    "{} · {details} · Ctrl+S save · Ctrl+Z undo · Ctrl+Q discard",
                    terminal_safe(status)
                )
            } else {
                format!("{details} · Ctrl+S save · Ctrl+Z undo · Ctrl+Y redo · Ctrl+W close tab")
            };
        }
        if let Some(status) = &self.status {
            return terminal_safe(status);
        }
        if let Some(preview) = &self.preview {
            if !preview.hunk_ranges.is_empty() {
                return if preview
                    .git_target
                    .as_ref()
                    .is_some_and(|target| target.staged)
                {
                    "[ ] hunk · u unstage hunk · d discard file · Esc back".into()
                } else {
                    "[ ] hunk · s stage hunk · d discard file · Esc back".into()
                };
            }
            if preview.git_target.is_some() {
                return if preview.edit_target.is_some() {
                    "e edit · d discard · Esc back".into()
                } else {
                    "d discard · Esc back".into()
                };
            }
            return if preview.edit_target.is_some() {
                "↑↓ scroll · e edit file · Esc back".into()
            } else {
                "↑↓ scroll  PgUp/PgDn page  Esc back".into()
            };
        }
        match self.view {
            View::Explorer => "↑↓ move  Enter open/expand  n new file  m menu  . hidden  r refresh  c collapse  2 Search  3 Source Control  q quit".into(),
            View::Search => "type search  Tab fields/results  ↑↓ select  Enter open  Ctrl+F focus  Esc back".into(),
            View::SourceControl => "↑↓ move  Enter expand/stage  a stage all  u unstage all  r refresh  2 Search  q quit".into(),
        }
    }
}

fn editor_line_spans(
    line: &[char],
    cursor_col: Option<usize>,
    selection: Option<(usize, usize)>,
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
        let selected = selection.is_some_and(|(start, end)| index >= start && index < end);
        let style = if cursor_col == Some(index) {
            Style::default().fg(BG).bg(ACCENT)
        } else if selected {
            Style::default().fg(FG).bg(SELECTED)
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

fn language_for_path(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "rs" => "Rust",
        "md" | "markdown" => "Markdown",
        "toml" => "TOML",
        "json" => "JSON",
        "py" => "Python",
        "js" | "mjs" | "cjs" => "JavaScript",
        "ts" | "tsx" => "TypeScript",
        "go" => "Go",
        "c" | "h" => "C",
        "cc" | "cpp" | "cxx" | "hpp" => "C++",
        "sh" | "bash" => "Shell",
        "yml" | "yaml" => "YAML",
        "html" | "htm" => "HTML",
        "css" => "CSS",
        "sql" => "SQL",
        "java" => "Java",
        _ => "Plain Text",
    }
}

fn copy_to_clipboard(text: &str) -> io::Result<()> {
    let encoded = base64(text.as_bytes());
    let mut stdout = io::stdout().lock();
    write!(stdout, "\x1b]52;c;{encoded}\x07")?;
    stdout.flush()
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = *chunk.get(1).unwrap_or(&0);
        let third = *chunk.get(2).unwrap_or(&0);
        result.push(TABLE[(first >> 2) as usize] as char);
        result.push(TABLE[(((first & 0b11) << 4) | (second >> 4)) as usize] as char);
        result.push(if chunk.len() > 1 {
            TABLE[(((second & 0b1111) << 2) | (third >> 6)) as usize] as char
        } else {
            '='
        });
        result.push(if chunk.len() > 2 {
            TABLE[(third & 0b11_1111) as usize] as char
        } else {
            '='
        });
    }
    result
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
fn split_diff_hunks(lines: &[String]) -> Vec<String> {
    let mut headers = Vec::new();
    let mut current = Vec::new();
    let mut hunks = Vec::new();
    for line in lines {
        if line.starts_with("@@") {
            if !current.is_empty() {
                hunks.push(format!("{}\n", current.join("\n")));
            }
            current = headers.clone();
            current.push(line.clone());
        } else if current.is_empty() {
            headers.push(line.clone());
        } else {
            current.push(line.clone());
        }
    }
    if !current.is_empty() {
        hunks.push(format!("{}\n", current.join("\n")));
    }
    hunks
}

fn git_diff(root: &Path, relative: &Path, staged: bool) -> Result<String, String> {
    let relative = relative.to_string_lossy();
    let diff = Git::discover(root)?.diff(&relative, staged)?;
    Ok(bounded_text(diff.as_bytes()))
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

fn row_item(
    row: &Row,
    theme: icons::IconTheme,
    selected: bool,
    drop_target: bool,
    decoration: Option<char>,
) -> ListItem<'static> {
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
    if let Some(decoration) = decoration {
        spans.push(Span::styled(
            format!(" {decoration}"),
            Style::default()
                .fg(git_color(decoration))
                .add_modifier(Modifier::BOLD),
        ));
    }
    if selected || drop_target {
        for span in &mut spans {
            *span = span.clone().style(span.style.bg(if selected {
                SELECTED
            } else {
                Color::Rgb(0x2a, 0x3d, 0x36)
            }));
        }
    }
    ListItem::new(Line::from(spans))
}

fn git_color(status: char) -> Color {
    match status {
        'A' | 'U' => Color::Rgb(0x8b, 0xd4, 0x9c),
        'D' | '!' => Color::Rgb(0xe4, 0x67, 0x6b),
        'R' | 'C' => Color::Rgb(0x82, 0xaa, 0xff),
        '•' => MUTED,
        _ => Color::Rgb(0xe5, 0xc0, 0x7b),
    }
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
        assert!(app.active_editor.is_some());
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
        assert!(
            app.active_editor.is_some(),
            "Esc must not discard unsaved text"
        );
        app.on_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL));
        assert!(app.active_editor.is_none());

        app.switch_view(View::Search, true);
        app.on_search_action(SearchAction::OpenHit("file.txt".into(), 1));
        assert!(app.active_editor.is_some());
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
        assert_eq!(app.preview.as_ref().unwrap().hunks.len(), 1);
        app.on_preview_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        let status = Git::discover(&root).unwrap().status().unwrap();
        assert_eq!(status.staged.len(), 1);
        assert!(status.unstaged.is_empty());
        app.on_scm_action(ScmAction::OpenFile {
            root: root.clone(),
            path: "change.txt".into(),
            staged: true,
        });
        app.on_preview_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE));
        let status = Git::discover(&root).unwrap().status().unwrap();
        assert!(status.staged.is_empty(), "{status:?}");
        assert_eq!(status.unstaged.len(), 1);
        app.on_preview_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
        assert!(app.active_editor.is_none(), "diff previews stay read-only");
        assert_eq!(
            std::fs::read_to_string(root.join("change.txt")).unwrap(),
            "after\n"
        );

        app.open_reference_preview(&root, DrawerRef::Commit("HEAD".into()));
        app.on_preview_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
        assert!(
            app.active_editor.is_none(),
            "commit previews stay read-only"
        );
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
    fn multiple_editors_preserve_buffers_and_close_tabs_safely() {
        let root = temp_dir("tabs");
        std::fs::create_dir_all(&root).unwrap();
        let first = root.join("first.rs");
        let second = root.join("second.md");
        std::fs::write(&first, "first").unwrap();
        std::fs::write(&second, "second").unwrap();
        let mut app = App::new(root.clone(), None);

        app.open_editor(&first, 0);
        app.on_editor_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        app.on_editor_key(KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE));
        app.open_editor(&second, 0);
        assert_eq!(app.editors.len(), 2);
        assert_eq!(app.active_editor, Some(1));
        app.on_key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::CONTROL));
        assert!(app.view == View::Explorer);
        assert_eq!(app.active_editor, Some(1));

        app.open_editor(&first, 0);
        assert_eq!(app.active_editor, Some(0));
        assert_eq!(
            app.active_editor().unwrap().lines()[0]
                .iter()
                .collect::<String>(),
            "first!"
        );
        app.on_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(
            app.active_editor,
            Some(0),
            "dirty tabs cannot be closed accidentally"
        );
        app.on_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL));
        assert_eq!(app.editors.len(), 1);
        assert_eq!(app.active_editor, Some(0));
        app.on_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert!(app.editors.is_empty());
        assert!(app.active_editor.is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn explorer_context_menu_opens_a_name_prompt_and_creates_folder() {
        let root = temp_dir("context-menu");
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("file.txt");
        std::fs::write(&file, "x").unwrap();
        let mut app = App::new(root.clone(), None);
        app.open_context_menu(file, false, Position::new(10, 5));
        app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        for ch in "created".chars() {
            app.on_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(root.join("created").is_dir());
        assert!(app.dialog.is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn clipboard_encoding_and_language_status_are_stable() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(language_for_path(Path::new("main.rs")), "Rust");
        assert_eq!(language_for_path(Path::new("README.md")), "Markdown");
        assert_eq!(language_for_path(Path::new("data.unknown")), "Plain Text");
    }

    #[test]
    fn explorer_file_operations_are_confined_and_keep_open_tabs_in_sync() {
        let root = temp_dir("file-ops");
        std::fs::create_dir_all(&root).unwrap();
        let mut app = App::new(root.clone(), None);

        assert!(app.create_file(&root, "../outside").is_err());
        let file = app.create_file(&root, "before.txt").unwrap().unwrap();
        assert!(file.is_file());
        assert!(app.create_file(&root, "before.txt").is_err());
        app.open_editor(&file, 0);

        let renamed = app.rename_path(&file, "after.txt").unwrap().unwrap();
        assert_eq!(app.active_editor().unwrap().path(), renamed);
        let folder = app.create_folder(&root, "target").unwrap().unwrap();
        app.move_path_into(&renamed, &folder);
        let moved = folder.join("after.txt");
        assert!(moved.is_file());
        assert_eq!(app.active_editor().unwrap().path(), moved);

        app.execute_confirm(ConfirmAction::DeletePath(folder.clone()));
        assert!(!folder.exists());
        assert!(app.editors.is_empty());
        assert!(app.active_editor.is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dirty_editor_blocks_git_worktree_switches() {
        let root = temp_dir("dirty-git-switch");
        let git_cmd = |args: &[&str]| {
            let output = Command::new("git")
                .current_dir(&root)
                .args(args)
                .output()
                .unwrap();
            assert!(output.status.success());
        };
        git_cmd(&["init", "-q", "-b", "main"]);
        git_cmd(&["config", "user.name", "teditor test"]);
        git_cmd(&["config", "user.email", "teditor@example.invalid"]);
        std::fs::write(root.join("file.txt"), "base\n").unwrap();
        git_cmd(&["add", "file.txt"]);
        git_cmd(&["commit", "-qm", "base"]);
        git_cmd(&["branch", "topic"]);

        let mut app = App::new(root.clone(), None);
        app.open_editor(&root.join("file.txt"), 0);
        app.active_editor_mut().unwrap().paste("unsaved");
        app.on_scm_action(ScmAction::SwitchBranch {
            root: root.clone(),
            name: "topic".into(),
        });
        assert!(app.active_editor().unwrap().dirty());
        assert_eq!(
            Git::discover(&root).unwrap().status().unwrap().branch,
            "main"
        );
        assert!(app.status.as_deref().unwrap().contains("Save or discard"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn clean_editor_tabs_reload_after_git_switches() {
        let root = temp_dir("clean-git-switch");
        let git_cmd = |args: &[&str]| {
            let output = Command::new("git")
                .current_dir(&root)
                .args(args)
                .output()
                .unwrap();
            assert!(output.status.success());
        };
        git_cmd(&["init", "-q", "-b", "main"]);
        git_cmd(&["config", "user.name", "teditor test"]);
        git_cmd(&["config", "user.email", "teditor@example.invalid"]);
        std::fs::write(root.join("file.txt"), "base\n").unwrap();
        git_cmd(&["add", "file.txt"]);
        git_cmd(&["commit", "-qm", "base"]);
        git_cmd(&["switch", "-qc", "topic"]);
        std::fs::write(root.join("file.txt"), "topic\n").unwrap();
        git_cmd(&["commit", "-qam", "topic update"]);
        git_cmd(&["switch", "-q", "main"]);

        let mut app = App::new(root.clone(), None);
        app.open_editor(&root.join("file.txt"), 0);
        app.on_scm_action(ScmAction::SwitchBranch {
            root: root.clone(),
            name: "topic".into(),
        });
        let text = app.active_editor().unwrap().lines()[0]
            .iter()
            .collect::<String>();
        assert_eq!(text, "topic");
        assert_eq!(
            Git::discover(&root).unwrap().status().unwrap().branch,
            "topic"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn preview_hunk_actions_stage_only_the_selected_change() {
        let root = temp_dir("partial-stage");
        let git_cmd = |args: &[&str]| {
            let output = Command::new("git")
                .current_dir(&root)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git_cmd(&["init", "-q", "-b", "main"]);
        git_cmd(&["config", "user.name", "teditor test"]);
        git_cmd(&["config", "user.email", "teditor@example.invalid"]);
        let base = (1..=20)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>();
        std::fs::write(root.join("file.txt"), format!("{}\n", base.join("\n"))).unwrap();
        git_cmd(&["add", "file.txt"]);
        git_cmd(&["commit", "-qm", "base"]);
        let mut changed = base;
        changed[1] = "changed two".into();
        changed[17] = "changed eighteen".into();
        std::fs::write(root.join("file.txt"), format!("{}\n", changed.join("\n"))).unwrap();

        let git = Git::discover(&root).unwrap();
        let diff = git.diff("file.txt", false).unwrap();
        let lines = diff.lines().map(str::to_owned).collect::<Vec<_>>();
        let hunks = split_diff_hunks(&lines);
        assert_eq!(hunks.len(), 2);
        git.apply_hunk(&hunks[0], false).unwrap();
        let staged = git.diff("file.txt", true).unwrap();
        let unstaged = git.diff("file.txt", false).unwrap();
        assert!(staged.contains("+changed two"));
        assert!(!staged.contains("+changed eighteen"));
        assert!(unstaged.contains("+changed eighteen"));
        assert!(!unstaged.contains("+changed two"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn split_diff_hunks_keep_file_headers_and_separate_changes() {
        let diff = [
            "diff --git a/file.txt b/file.txt",
            "index 1111111..2222222 100644",
            "--- a/file.txt",
            "+++ b/file.txt",
            "@@ -1 +1 @@",
            "-before one",
            "+after one",
            "@@ -9 +9 @@",
            "-before two",
            "+after two",
        ]
        .map(str::to_owned);
        let hunks = split_diff_hunks(&diff);
        assert_eq!(hunks.len(), 2);
        assert!(hunks[0].starts_with("diff --git a/file.txt b/file.txt\n"));
        assert!(hunks[1].starts_with("diff --git a/file.txt b/file.txt\n"));
        assert!(hunks[0].contains("+after one"));
        assert!(!hunks[0].contains("+after two"));
        assert!(hunks[1].contains("+after two"));
        assert!(!hunks[1].contains("+after one"));
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
