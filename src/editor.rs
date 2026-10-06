use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};
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

pub struct Editor {
    path: PathBuf,
    lines: Vec<Vec<char>>,
    cursor_line: usize,
    cursor_col: usize,
    preferred_col: usize,
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

    pub fn lines(&self) -> &[Vec<char>] {
        &self.lines
    }

    pub fn cursor_line(&self) -> usize {
        self.cursor_line
    }

    pub fn cursor_col(&self) -> usize {
        self.cursor_col
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

    pub fn place_cursor(&mut self, line: usize, cell: usize) {
        self.cursor_line = line.min(self.lines.len().saturating_sub(1));
        self.cursor_col = column_at_cell(&self.lines[self.cursor_line], cell);
        self.preferred_col = self.cursor_col;
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
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left)
                if body.contains(Position::new(mouse.column, mouse.row)) =>
            {
                let line = self.scroll + usize::from(mouse.row.saturating_sub(body.y));
                let cell = self.horizontal_scroll
                    + usize::from(mouse.column.saturating_sub(body.x.saturating_add(6)));
                self.place_cursor(line, cell);
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
            match key.code {
                KeyCode::Char('s' | 'S') => return EditAction::Save,
                KeyCode::Char('q' | 'Q') => return EditAction::Discard,
                KeyCode::Char('r' | 'R') => return EditAction::Reload,
                _ => return EditAction::None,
            }
        }

        match key.code {
            KeyCode::Esc => EditAction::Close,
            KeyCode::Left => {
                self.move_left();
                EditAction::None
            }
            KeyCode::Right => {
                self.move_right();
                EditAction::None
            }
            KeyCode::Up => {
                self.cursor_line = self.cursor_line.saturating_sub(1);
                self.cursor_col = self.preferred_col.min(self.line_len(self.cursor_line));
                EditAction::None
            }
            KeyCode::Down => {
                self.cursor_line = (self.cursor_line + 1).min(self.lines.len().saturating_sub(1));
                self.cursor_col = self.preferred_col.min(self.line_len(self.cursor_line));
                EditAction::None
            }
            KeyCode::PageUp => {
                self.cursor_line = self.cursor_line.saturating_sub(page.max(1));
                self.cursor_col = self.preferred_col.min(self.line_len(self.cursor_line));
                EditAction::None
            }
            KeyCode::PageDown => {
                self.cursor_line =
                    (self.cursor_line + page.max(1)).min(self.lines.len().saturating_sub(1));
                self.cursor_col = self.preferred_col.min(self.line_len(self.cursor_line));
                EditAction::None
            }
            KeyCode::Home => {
                self.cursor_col = 0;
                self.preferred_col = 0;
                EditAction::None
            }
            KeyCode::End => {
                self.cursor_col = self.line_len(self.cursor_line);
                self.preferred_col = self.cursor_col;
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
                let rest = self.lines[self.cursor_line].split_off(self.cursor_col);
                self.lines.insert(self.cursor_line + 1, rest);
                self.cursor_line += 1;
                self.cursor_col = 0;
                self.changed();
                EditAction::None
            }
            KeyCode::Tab => {
                for _ in 0..TAB_WIDTH {
                    self.insert_char(' ');
                }
                EditAction::None
            }
            KeyCode::Char(ch) if !ch.is_control() && (!shortcut || (ctrl && alt)) => {
                self.insert_char(ch);
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

    fn line_len(&self, line: usize) -> usize {
        self.lines[line].len()
    }

    fn changed(&mut self) {
        self.dirty = true;
        self.preferred_col = self.cursor_col;
    }

    fn insert_char(&mut self, ch: char) {
        self.lines[self.cursor_line].insert(self.cursor_col, ch);
        self.cursor_col += 1;
        self.changed();
    }

    fn backspace(&mut self) {
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
            self.lines[self.cursor_line].remove(self.cursor_col);
            self.changed();
        } else if self.cursor_line > 0 {
            let current = self.lines.remove(self.cursor_line);
            self.cursor_line -= 1;
            self.cursor_col = self.lines[self.cursor_line].len();
            self.lines[self.cursor_line].extend(current);
            self.changed();
        }
    }

    fn delete_forward(&mut self) {
        if self.cursor_col < self.line_len(self.cursor_line) {
            self.lines[self.cursor_line].remove(self.cursor_col);
            self.changed();
        } else if self.cursor_line + 1 < self.lines.len() {
            let next = self.lines.remove(self.cursor_line + 1);
            self.lines[self.cursor_line].extend(next);
            self.changed();
        }
    }

    fn move_left(&mut self) {
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
        } else if self.cursor_line > 0 {
            self.cursor_line -= 1;
            self.cursor_col = self.line_len(self.cursor_line);
        }
        self.preferred_col = self.cursor_col;
    }

    fn move_right(&mut self) {
        if self.cursor_col < self.line_len(self.cursor_line) {
            self.cursor_col += 1;
        } else if self.cursor_line + 1 < self.lines.len() {
            self.cursor_line += 1;
            self.cursor_col = 0;
        }
        self.preferred_col = self.cursor_col;
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
