use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position as MousePosition, Rect};
use unicode_width::UnicodeWidthChar;

const TAB_WIDTH: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditAction {
    None,
    Close,
    Save,
    Reload,
    Discard,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveOutcome {
    Saved,
    Conflict,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LineEnding {
    Lf,
    CrLf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Position {
    pub line: usize,
    pub col: usize,
}

#[derive(Clone)]
struct HistoryEntry {
    start: Position,
    before: String,
    after: String,
    before_cursor: Position,
    after_cursor: Position,
}

const HISTORY_LIMIT: usize = 200;
const HISTORY_BYTES_LIMIT: usize = 4 * 1024 * 1024;

pub struct Editor {
    path: PathBuf,
    lines: Vec<Vec<char>>,
    cursor_line: usize,
    cursor_col: usize,
    preferred_col: usize,
    selection_anchor: Option<Position>,
    undo: Vec<HistoryEntry>,
    redo: Vec<HistoryEntry>,
    scroll: usize,
    horizontal_scroll: usize,
    original_bytes: Vec<u8>,
    bom: bool,
    line_ending: LineEnding,
    dirty: bool,
    external_changed: bool,
}

impl Editor {
    pub fn open(path: &Path, max_bytes: usize) -> Result<Self, String> {
        let bytes = read_limited(path, max_bytes)?;
        if bytes.contains(&0) {
            return Err("Binary files cannot be edited".into());
        }
        let bom = bytes.starts_with(&[0xef, 0xbb, 0xbf]);
        let text_bytes = if bom { &bytes[3..] } else { &bytes };
        let text =
            std::str::from_utf8(text_bytes).map_err(|_| "File is not valid UTF-8".to_string())?;
        let line_ending = if text.contains("\r\n") {
            LineEnding::CrLf
        } else {
            LineEnding::Lf
        };
        // ponytail: mixed line endings normalize to the file's detected style; preserve per-line endings only if users need mixed-file fidelity.
        let normalized = text.replace("\r\n", "\n");
        let lines = normalized
            .split('\n')
            .map(|line| line.chars().collect())
            .collect();
        Ok(Self {
            path: path.to_path_buf(),
            lines,
            cursor_line: 0,
            cursor_col: 0,
            preferred_col: 0,
            selection_anchor: None,
            undo: Vec::new(),
            redo: Vec::new(),
            scroll: 0,
            horizontal_scroll: 0,
            original_bytes: bytes,
            bom,
            line_ending,
            dirty: false,
            external_changed: false,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn rename_path(&mut self, path: PathBuf) {
        self.path = path;
    }

    pub fn lines(&self) -> &[Vec<char>] {
        &self.lines
    }

    pub fn cursor_line(&self) -> usize {
        self.cursor_line
    }

    pub fn cursor_col(&self) -> usize {
        self.cursor_col
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    pub fn selection(&self) -> Option<(Position, Position)> {
        self.selection_anchor
            .filter(|anchor| *anchor != self.position())
            .map(|anchor| {
                if anchor < self.position() {
                    (anchor, self.position())
                } else {
                    (self.position(), anchor)
                }
            })
    }

    pub fn selected_text(&self) -> Option<String> {
        let (start, end) = self.selection()?;
        Some(self.text_between(start, end))
    }

    pub fn select_all(&mut self) {
        self.selection_anchor = Some(Position { line: 0, col: 0 });
        self.set_cursor(Position {
            line: self.lines.len().saturating_sub(1),
            col: self.lines.last().map_or(0, Vec::len),
        });
    }

    pub fn cut_selection(&mut self) -> Option<String> {
        let text = self.selected_text()?;
        let (start, end) = self.selection()?;
        self.apply_text_edit(start, end, "");
        Some(text)
    }

    pub fn paste(&mut self, text: &str) {
        if !text.is_empty() {
            self.replace_selection(text);
        }
    }

    pub fn undo(&mut self) -> bool {
        let Some(entry) = self.undo.pop() else {
            return false;
        };
        let end = position_after(entry.start, &entry.after);
        self.replace_range(entry.start, end, &entry.before);
        self.selection_anchor = None;
        self.set_cursor(entry.before_cursor);
        self.redo.push(entry);
        self.update_dirty();
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(entry) = self.redo.pop() else {
            return false;
        };
        let end = position_after(entry.start, &entry.before);
        self.replace_range(entry.start, end, &entry.after);
        self.selection_anchor = None;
        self.set_cursor(entry.after_cursor);
        self.undo.push(entry);
        self.update_dirty();
        true
    }

    pub fn scroll(&self) -> usize {
        self.scroll
    }

    pub fn horizontal_scroll(&self) -> usize {
        self.horizontal_scroll
    }

    pub fn dirty(&self) -> bool {
        self.dirty
    }

    pub fn external_changed(&self) -> bool {
        self.external_changed
    }

    pub fn has_bom(&self) -> bool {
        self.bom
    }

    pub fn line_ending(&self) -> &'static str {
        match self.line_ending {
            LineEnding::Lf => "LF",
            LineEnding::CrLf => "CRLF",
        }
    }

    pub fn place_cursor(&mut self, line: usize, cell: usize) {
        let line = line.min(self.lines.len().saturating_sub(1));
        let col = column_at_cell(&self.lines[line], cell);
        self.selection_anchor = None;
        self.set_cursor(Position { line, col });
    }

    pub fn scroll_by(&mut self, delta: isize, height: usize) {
        let max = self.lines.len().saturating_sub(height.max(1));
        self.scroll = self.scroll.saturating_add_signed(delta).min(max);
    }

    /// Keep the insertion point visible in the editor's current viewport.
    pub fn ensure_visible(&mut self, width: usize, height: usize) {
        let height = height.max(1);
        if self.cursor_line < self.scroll {
            self.scroll = self.cursor_line;
        } else if self.cursor_line >= self.scroll + height {
            self.scroll = self.cursor_line + 1 - height;
        }
        self.scroll = self.scroll.min(self.lines.len().saturating_sub(height));

        let content_width = width.saturating_sub(7).max(1);
        let cursor_cell = cells_width(&self.lines[self.cursor_line][..self.cursor_col]);
        if cursor_cell < self.horizontal_scroll {
            self.horizontal_scroll = cursor_cell;
        } else if cursor_cell >= self.horizontal_scroll + content_width {
            self.horizontal_scroll = cursor_cell + 1 - content_width;
        }
    }

    pub fn on_mouse(&mut self, mouse: MouseEvent, body: Rect) {
        let position = || {
            let line = self.scroll + usize::from(mouse.row.saturating_sub(body.y));
            let cell = self.horizontal_scroll
                + usize::from(mouse.column.saturating_sub(body.x.saturating_add(6)));
            Position {
                line: line.min(self.lines.len().saturating_sub(1)),
                col: column_at_cell(
                    &self.lines[line.min(self.lines.len().saturating_sub(1))],
                    cell,
                ),
            }
        };
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left)
                if body.contains(MousePosition::new(mouse.column, mouse.row)) =>
            {
                let position = position();
                self.selection_anchor = Some(position);
                self.set_cursor(position);
            }
            MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left)
                if body.contains(MousePosition::new(mouse.column, mouse.row)) =>
            {
                self.selection_anchor.get_or_insert(self.position());
                self.set_cursor(position());
            }
            MouseEventKind::ScrollUp => {
                self.scroll_by(-3, usize::from(body.height));
                self.cursor_line = self.scroll.min(self.lines.len().saturating_sub(1));
                self.cursor_col = self.cursor_col.min(self.line_len(self.cursor_line));
                self.preferred_col = self.cursor_col;
            }
            MouseEventKind::ScrollDown => {
                self.scroll_by(3, usize::from(body.height));
                self.cursor_line = self.scroll.min(self.lines.len().saturating_sub(1));
                self.cursor_col = self.cursor_col.min(self.line_len(self.cursor_line));
                self.preferred_col = self.cursor_col;
            }
            _ => {}
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent, page: usize) -> EditAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shortcut = (ctrl && !alt) || key.modifiers.contains(KeyModifiers::SUPER);
        if shortcut {
            if matches!(key.code, KeyCode::Char('z' | 'Z')) {
                if key.modifiers.contains(KeyModifiers::SHIFT) {
                    self.redo();
                } else {
                    self.undo();
                }
                return EditAction::None;
            }
            match key.code {
                KeyCode::Char('s' | 'S') => return EditAction::Save,
                KeyCode::Char('q' | 'Q') => return EditAction::Discard,
                KeyCode::Char('r' | 'R') => return EditAction::Reload,
                KeyCode::Char('y' | 'Y') => {
                    self.redo();
                    return EditAction::None;
                }
                KeyCode::Char('a' | 'A') => {
                    self.select_all();
                    return EditAction::None;
                }
                _ => return EditAction::None,
            }
        }

        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Esc => EditAction::Close,
            KeyCode::Left => {
                self.move_left(shift);
                EditAction::None
            }
            KeyCode::Right => {
                self.move_right(shift);
                EditAction::None
            }
            KeyCode::Up => {
                let preferred = self.preferred_col;
                let line = self.cursor_line.saturating_sub(1);
                self.move_cursor_to(
                    Position {
                        line,
                        col: preferred.min(self.line_len(line)),
                    },
                    shift,
                );
                self.preferred_col = preferred;
                EditAction::None
            }
            KeyCode::Down => {
                let preferred = self.preferred_col;
                let line = (self.cursor_line + 1).min(self.lines.len().saturating_sub(1));
                self.move_cursor_to(
                    Position {
                        line,
                        col: preferred.min(self.line_len(line)),
                    },
                    shift,
                );
                self.preferred_col = preferred;
                EditAction::None
            }
            KeyCode::PageUp => {
                let preferred = self.preferred_col;
                let line = self.cursor_line.saturating_sub(page.max(1));
                self.move_cursor_to(
                    Position {
                        line,
                        col: preferred.min(self.line_len(line)),
                    },
                    shift,
                );
                self.preferred_col = preferred;
                EditAction::None
            }
            KeyCode::PageDown => {
                let preferred = self.preferred_col;
                let line = (self.cursor_line + page.max(1)).min(self.lines.len().saturating_sub(1));
                self.move_cursor_to(
                    Position {
                        line,
                        col: preferred.min(self.line_len(line)),
                    },
                    shift,
                );
                self.preferred_col = preferred;
                EditAction::None
            }
            KeyCode::Home => {
                self.move_cursor_to(
                    Position {
                        line: self.cursor_line,
                        col: 0,
                    },
                    shift,
                );
                EditAction::None
            }
            KeyCode::End => {
                self.move_cursor_to(
                    Position {
                        line: self.cursor_line,
                        col: self.line_len(self.cursor_line),
                    },
                    shift,
                );
                EditAction::None
            }
            KeyCode::Backspace => {
                self.backspace();
                EditAction::None
            }
            KeyCode::Delete => {
                self.delete_forward();
                EditAction::None
            }
            KeyCode::Enter => {
                self.replace_selection("\n");
                EditAction::None
            }
            KeyCode::Tab => {
                self.replace_selection(&" ".repeat(TAB_WIDTH));
                EditAction::None
            }
            KeyCode::Char(ch) if !ch.is_control() && (!shortcut || (ctrl && alt)) => {
                self.replace_selection(&ch.to_string());
                EditAction::None
            }
            _ => EditAction::None,
        }
    }

    pub fn save(&mut self, force: bool) -> Result<SaveOutcome, String> {
        if !self.dirty {
            return Ok(SaveOutcome::Saved);
        }
        if !force {
            match read_limited(&self.path, self.original_bytes.len().max(1)) {
                Ok(current) if current == self.original_bytes => {}
                Ok(_) | Err(_) => {
                    self.external_changed = true;
                    return Ok(SaveOutcome::Conflict);
                }
            }
        }
        let bytes = self.encoded_bytes();
        atomic_write(&self.path, &bytes).map_err(|error| format!("Unable to save: {error}"))?;
        self.original_bytes = bytes;
        self.dirty = false;
        self.external_changed = false;
        Ok(SaveOutcome::Saved)
    }

    pub fn reload(&mut self, max_bytes: usize) -> Result<(), String> {
        *self = Self::open(&self.path, max_bytes)?;
        Ok(())
    }

    pub fn reload_if_changed(&mut self, max_bytes: usize) -> Result<bool, String> {
        let current = read_limited(&self.path, max_bytes).map_err(|error| error.to_string())?;
        if current == self.original_bytes {
            return Ok(false);
        }
        self.reload(max_bytes)?;
        Ok(true)
    }

    fn position(&self) -> Position {
        Position {
            line: self.cursor_line,
            col: self.cursor_col,
        }
    }

    fn set_cursor(&mut self, position: Position) {
        self.cursor_line = position.line.min(self.lines.len().saturating_sub(1));
        self.cursor_col = position.col.min(self.line_len(self.cursor_line));
        self.preferred_col = self.cursor_col;
    }

    fn move_cursor_to(&mut self, position: Position, extend: bool) {
        let old = self.position();
        if extend {
            self.selection_anchor.get_or_insert(old);
        } else {
            self.selection_anchor = None;
        }
        self.set_cursor(position);
    }

    fn line_len(&self, line: usize) -> usize {
        self.lines[line].len()
    }

    fn text_between(&self, start: Position, end: Position) -> String {
        if start.line == end.line {
            return self.lines[start.line][start.col..end.col].iter().collect();
        }
        let mut parts = Vec::with_capacity(end.line - start.line + 1);
        parts.push(
            self.lines[start.line][start.col..]
                .iter()
                .collect::<String>(),
        );
        parts.extend(
            self.lines[start.line + 1..end.line]
                .iter()
                .map(|line| line.iter().collect()),
        );
        parts.push(self.lines[end.line][..end.col].iter().collect());
        parts.join("\n")
    }

    fn replace_range(&mut self, start: Position, end: Position, text: &str) -> Position {
        let prefix = self.lines[start.line][..start.col].to_vec();
        let suffix = self.lines[end.line][end.col..].to_vec();
        let parts = text
            .split('\n')
            .map(|part| part.chars().collect::<Vec<_>>())
            .collect::<Vec<_>>();
        if parts.len() == 1 {
            let mut line = prefix;
            line.extend_from_slice(&parts[0]);
            let position = Position {
                line: start.line,
                col: line.len(),
            };
            line.extend(suffix);
            self.lines.splice(start.line..=end.line, [line]);
            position
        } else {
            let mut replacement = Vec::with_capacity(parts.len());
            let mut first = prefix;
            first.extend_from_slice(&parts[0]);
            replacement.push(first);
            replacement.extend(parts[1..parts.len() - 1].iter().cloned());
            let mut last = parts.last().expect("split has at least one part").clone();
            let position = Position {
                line: start.line + parts.len() - 1,
                col: last.len(),
            };
            last.extend(suffix);
            replacement.push(last);
            self.lines.splice(start.line..=end.line, replacement);
            position
        }
    }

    fn replace_selection(&mut self, text: &str) {
        let (start, end) = self
            .selection()
            .unwrap_or((self.position(), self.position()));
        self.apply_text_edit(start, end, text);
    }

    fn apply_text_edit(&mut self, start: Position, end: Position, after: &str) {
        let before = self.text_between(start, end);
        if before == after {
            self.selection_anchor = None;
            return;
        }
        let before_cursor = self.position();
        let after_cursor = self.replace_range(start, end, after);
        self.selection_anchor = None;
        self.set_cursor(after_cursor);
        self.changed();
        self.redo.clear();
        self.undo.push(HistoryEntry {
            start,
            before,
            after: after.to_string(),
            before_cursor,
            after_cursor,
        });
        self.trim_history();
    }

    fn trim_history(&mut self) {
        let mut bytes = self
            .undo
            .iter()
            .map(|entry| entry.before.len() + entry.after.len())
            .sum::<usize>();
        while self.undo.len() > HISTORY_LIMIT || bytes > HISTORY_BYTES_LIMIT {
            let Some(entry) = self.undo.first() else {
                break;
            };
            bytes = bytes.saturating_sub(entry.before.len() + entry.after.len());
            self.undo.remove(0);
        }
        // ponytail: undo history is capped at 4 MiB; raise this if large-paste history matters.
    }

    fn update_dirty(&mut self) {
        self.dirty = self.encoded_bytes() != self.original_bytes;
    }

    fn changed(&mut self) {
        self.dirty = true;
        self.preferred_col = self.cursor_col;
    }

    fn backspace(&mut self) {
        if let Some((start, end)) = self.selection() {
            self.apply_text_edit(start, end, "");
        } else {
            let end = self.position();
            let start = previous_position(end, &self.lines);
            if start != end {
                self.apply_text_edit(start, end, "");
            }
        }
    }

    fn delete_forward(&mut self) {
        if let Some((start, end)) = self.selection() {
            self.apply_text_edit(start, end, "");
        } else {
            let start = self.position();
            let end = next_position(start, &self.lines);
            if start != end {
                self.apply_text_edit(start, end, "");
            }
        }
    }

    fn move_left(&mut self, extend: bool) {
        self.move_cursor_to(previous_position(self.position(), &self.lines), extend);
    }

    fn move_right(&mut self, extend: bool) {
        self.move_cursor_to(next_position(self.position(), &self.lines), extend);
    }

    fn encoded_bytes(&self) -> Vec<u8> {
        let newline = match self.line_ending {
            LineEnding::Lf => "\n",
            LineEnding::CrLf => "\r\n",
        };
        let text = self
            .lines
            .iter()
            .map(|line| line.iter().collect::<String>())
            .collect::<Vec<_>>()
            .join(newline);
        let mut bytes = Vec::with_capacity(text.len() + usize::from(self.bom) * 3);
        if self.bom {
            bytes.extend_from_slice(&[0xef, 0xbb, 0xbf]);
        }
        bytes.extend_from_slice(text.as_bytes());
        bytes
    }
}

fn read_limited(path: &Path, max_bytes: usize) -> Result<Vec<u8>, String> {
    let file = fs::File::open(path).map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    file.take(max_bytes.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() > max_bytes {
        return Err(format!("File exceeds the {max_bytes}-byte edit limit"));
    }
    Ok(bytes)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "file path has no name"))?;
    let permissions = fs::metadata(path)
        .ok()
        .map(|metadata| metadata.permissions());

    for attempt in 0..16 {
        let temp = parent.join(format!(
            ".{}.teditor-{}-{attempt}.tmp",
            name.to_string_lossy(),
            std::process::id()
        ));
        let file = match OpenOptions::new().write(true).create_new(true).open(&temp) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        let result = (|| {
            if let Some(permissions) = permissions.clone() {
                file.set_permissions(permissions)?;
            }
            let mut file = file;
            file.write_all(bytes)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temp, path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        return result;
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not create a temporary save file",
    ))
}

fn previous_position(position: Position, lines: &[Vec<char>]) -> Position {
    if position.col > 0 {
        Position {
            col: position.col - 1,
            ..position
        }
    } else if position.line > 0 {
        Position {
            line: position.line - 1,
            col: lines[position.line - 1].len(),
        }
    } else {
        position
    }
}

fn next_position(position: Position, lines: &[Vec<char>]) -> Position {
    if position.col < lines[position.line].len() {
        Position {
            col: position.col + 1,
            ..position
        }
    } else if position.line + 1 < lines.len() {
        Position {
            line: position.line + 1,
            col: 0,
        }
    } else {
        position
    }
}

fn position_after(start: Position, text: &str) -> Position {
    let mut parts = text.split('\n');
    let first = parts.next().unwrap_or_default();
    let mut line = start.line;
    let mut col = start.col + first.chars().count();
    for part in parts {
        line += 1;
        col = part.chars().count();
    }
    Position { line, col }
}

fn cells_width(chars: &[char]) -> usize {
    chars.iter().fold(0, |width, ch| {
        width
            + if *ch == '\t' {
                TAB_WIDTH - width % TAB_WIDTH
            } else {
                ch.width().unwrap_or(0)
            }
    })
}

fn column_at_cell(line: &[char], target: usize) -> usize {
    let mut width = 0;
    for (index, ch) in line.iter().enumerate() {
        if width >= target {
            return index;
        }
        width += if *ch == '\t' {
            TAB_WIDTH - width % TAB_WIDTH
        } else {
            ch.width().unwrap_or(0)
        };
    }
    line.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;

    fn key(editor: &mut Editor, code: KeyCode, modifiers: KeyModifiers) -> EditAction {
        editor.handle_key(KeyEvent::new(code, modifiers), 20)
    }

    #[test]
    fn selection_clipboard_edits_and_undo_redo_round_trip_multiline_text() {
        let root =
            std::env::temp_dir().join(format!("teditor-editor-history-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let path = root.join("text.txt");
        fs::write(&path, "ab\ncd").unwrap();
        let mut editor = Editor::open(&path, 1024).unwrap();
        key(&mut editor, KeyCode::Char('a'), KeyModifiers::CONTROL);
        assert_eq!(editor.selected_text().as_deref(), Some("ab\ncd"));
        assert_eq!(editor.cut_selection().as_deref(), Some("ab\ncd"));
        assert_eq!(editor.lines().len(), 1);
        assert!(editor.dirty());
        assert!(editor.undo());
        assert_eq!(editor.encoded_bytes(), b"ab\ncd");
        assert!(!editor.dirty());
        assert!(editor.redo());
        assert_eq!(editor.encoded_bytes(), b"");
        editor.paste("first\nsecond");
        assert_eq!(editor.encoded_bytes(), b"first\nsecond");
        assert!(editor.undo());
        assert_eq!(editor.encoded_bytes(), b"");
        assert!(editor.redo());
        assert_eq!(editor.encoded_bytes(), b"first\nsecond");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn shift_navigation_selects_and_typing_replaces_the_selection() {
        let root =
            std::env::temp_dir().join(format!("teditor-editor-selection-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let path = root.join("text.txt");
        fs::write(&path, "abc").unwrap();
        let mut editor = Editor::open(&path, 1024).unwrap();
        editor.place_cursor(0, 3);
        key(&mut editor, KeyCode::Left, KeyModifiers::SHIFT);
        assert_eq!(editor.selected_text().as_deref(), Some("c"));
        key(&mut editor, KeyCode::Char('x'), KeyModifiers::NONE);
        assert_eq!(editor.encoded_bytes(), b"abx");
        assert!(editor.undo());
        assert_eq!(editor.encoded_bytes(), b"abc");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn deleting_and_inserting_newlines_are_reversible() {
        let root =
            std::env::temp_dir().join(format!("teditor-editor-newline-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let path = root.join("text.txt");
        fs::write(&path, "ab\ncd").unwrap();
        let mut editor = Editor::open(&path, 1024).unwrap();
        editor.place_cursor(0, 2);
        key(&mut editor, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(editor.encoded_bytes(), b"ab\n\ncd");
        assert!(editor.undo());
        assert_eq!(editor.encoded_bytes(), b"ab\ncd");
        editor.place_cursor(0, 2);
        key(&mut editor, KeyCode::Delete, KeyModifiers::NONE);
        assert_eq!(editor.encoded_bytes(), b"abcd");
        assert!(editor.undo());
        assert_eq!(editor.encoded_bytes(), b"ab\ncd");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn unicode_edit_save_preserves_bom_crlf_and_detects_external_change() {
        let root = std::env::temp_dir().join(format!("teditor-editor-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let path = root.join("text.txt");
        fs::write(&path, b"\xef\xbb\xbfalpha\r\nbeta\r\n").unwrap();
        let mut editor = Editor::open(&path, 1024).unwrap();
        key(&mut editor, KeyCode::End, KeyModifiers::NONE);
        key(&mut editor, KeyCode::Char('界'), KeyModifiers::NONE);
        assert_eq!(
            key(&mut editor, KeyCode::Char('s'), KeyModifiers::CONTROL),
            EditAction::Save
        );
        assert_eq!(editor.save(false).unwrap(), SaveOutcome::Saved);
        assert_eq!(
            fs::read(&path).unwrap(),
            [
                b"\xef\xbb\xbfalpha".as_slice(),
                "界".as_bytes(),
                b"\r\nbeta\r\n"
            ]
            .concat()
        );

        key(&mut editor, KeyCode::Char('!'), KeyModifiers::NONE);
        fs::write(&path, b"external\n").unwrap();
        assert_eq!(editor.save(false).unwrap(), SaveOutcome::Conflict);
        assert_eq!(fs::read(&path).unwrap(), b"external\n");
        assert_eq!(editor.save(true).unwrap(), SaveOutcome::Saved);
        assert!(String::from_utf8_lossy(&fs::read(&path).unwrap()).contains("alpha界!"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn split_merge_and_unicode_cursor_use_character_columns() {
        let root =
            std::env::temp_dir().join(format!("teditor-editor-lines-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let path = root.join("text.txt");
        fs::write(&path, "a界b").unwrap();
        let mut editor = Editor::open(&path, 1024).unwrap();
        editor.place_cursor(0, 3);
        key(&mut editor, KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(editor.lines()[0].iter().collect::<String>(), "ab");
        key(&mut editor, KeyCode::Enter, KeyModifiers::NONE);
        key(&mut editor, KeyCode::Char('x'), KeyModifiers::NONE);
        assert_eq!(editor.lines()[1].iter().collect::<String>(), "xb");
        key(&mut editor, KeyCode::Delete, KeyModifiers::NONE);
        assert_eq!(editor.lines()[1].iter().collect::<String>(), "x");
        fs::remove_dir_all(root).unwrap();
    }
}
