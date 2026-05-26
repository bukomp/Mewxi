//! Single-line text editor with readline-style cursor movement and
//! edit shortcuts. Shared between the session-view driver input and the
//! new-session modal path bar so both grow the same keymap.
//!
//! The dispatcher [`TextInput::handle_edit_key`] only claims keys that
//! are unambiguously *edit* operations — Enter, Esc, Tab/BackTab, the
//! arrow keys without modifiers other than CTRL/ALT, Ctrl-C, Ctrl-D,
//! and Ctrl-`.` all fall through so each caller keeps its own meaning
//! for them.
//!
//! Word boundary is the transition between a *wordy* char
//! (`is_alphanumeric() || == '_'`) and a non-wordy char. This makes
//! `/`, `.`, `-`, and whitespace all word breaks, so e.g. Ctrl-W on a
//! path like `/home/foo/bar` deletes the trailing segment instead of
//! the whole string.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Default, Clone)]
pub struct TextInput {
    buf: String,
    cursor: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditOutcome {
    Consumed { changed: bool },
    Passthrough,
}

impl TextInput {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_str(s: &str) -> Self {
        let buf = s.to_owned();
        let cursor = buf.len();
        Self { buf, cursor }
    }

    pub fn set(&mut self, s: String) {
        self.buf = s;
        self.cursor = self.buf.len();
    }

    pub fn clear(&mut self) {
        self.buf.clear();
        self.cursor = 0;
    }

    pub fn as_str(&self) -> &str {
        &self.buf
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn cursor_byte(&self) -> usize {
        self.cursor
    }

    /// Char index of the cursor (counts from the start). Useful for
    /// renderers that work in character columns rather than bytes.
    pub fn cursor_char(&self) -> usize {
        self.buf[..self.cursor].chars().count()
    }

    pub fn insert_char(&mut self, c: char) -> bool {
        self.buf.insert(self.cursor, c);
        self.cursor += c.len_utf8();
        true
    }

    pub fn backspace(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let prev = prev_boundary(&self.buf, self.cursor);
        self.buf.replace_range(prev..self.cursor, "");
        self.cursor = prev;
        true
    }

    pub fn delete_forward(&mut self) -> bool {
        if self.cursor >= self.buf.len() {
            return false;
        }
        let next = next_boundary(&self.buf, self.cursor);
        self.buf.replace_range(self.cursor..next, "");
        true
    }

    pub fn delete_word_back(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let target = word_boundary_left(&self.buf, self.cursor);
        self.buf.replace_range(target..self.cursor, "");
        self.cursor = target;
        true
    }

    pub fn delete_word_forward(&mut self) -> bool {
        if self.cursor >= self.buf.len() {
            return false;
        }
        let target = word_boundary_right(&self.buf, self.cursor);
        self.buf.replace_range(self.cursor..target, "");
        true
    }

    pub fn delete_to_start(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.buf.replace_range(..self.cursor, "");
        self.cursor = 0;
        true
    }

    pub fn delete_to_end(&mut self) -> bool {
        if self.cursor >= self.buf.len() {
            return false;
        }
        self.buf.truncate(self.cursor);
        true
    }

    pub fn move_left(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.cursor = prev_boundary(&self.buf, self.cursor);
        true
    }

    pub fn move_right(&mut self) -> bool {
        if self.cursor >= self.buf.len() {
            return false;
        }
        self.cursor = next_boundary(&self.buf, self.cursor);
        true
    }

    pub fn move_word_left(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.cursor = word_boundary_left(&self.buf, self.cursor);
        true
    }

    pub fn move_word_right(&mut self) -> bool {
        if self.cursor >= self.buf.len() {
            return false;
        }
        self.cursor = word_boundary_right(&self.buf, self.cursor);
        true
    }

    pub fn move_home(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.cursor = 0;
        true
    }

    pub fn move_end(&mut self) -> bool {
        if self.cursor == self.buf.len() {
            return false;
        }
        self.cursor = self.buf.len();
        true
    }

    pub fn handle_edit_key(&mut self, k: KeyEvent) -> EditOutcome {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        match k.code {
            KeyCode::Backspace if alt => consumed(self.delete_word_back()),
            KeyCode::Backspace if !ctrl => consumed(self.backspace()),
            KeyCode::Delete => consumed(self.delete_forward()),
            KeyCode::Left if ctrl || alt => consumed(self.move_word_left()),
            KeyCode::Left if !ctrl && !alt => consumed(self.move_left()),
            KeyCode::Right if ctrl || alt => consumed(self.move_word_right()),
            KeyCode::Right if !ctrl && !alt => consumed(self.move_right()),
            KeyCode::Home => consumed(self.move_home()),
            KeyCode::End => consumed(self.move_end()),
            KeyCode::Char(c) if ctrl && !alt => match c {
                'a' => consumed(self.move_home()),
                'e' => consumed(self.move_end()),
                'b' => consumed(self.move_left()),
                'f' => consumed(self.move_right()),
                'h' => consumed(self.backspace()),
                'w' => consumed(self.delete_word_back()),
                'u' => consumed(self.delete_to_start()),
                'k' => consumed(self.delete_to_end()),
                _ => EditOutcome::Passthrough,
            },
            KeyCode::Char(c) if alt && !ctrl => match c {
                'b' => consumed(self.move_word_left()),
                'f' => consumed(self.move_word_right()),
                'd' => consumed(self.delete_word_forward()),
                _ => EditOutcome::Passthrough,
            },
            KeyCode::Char(c) if !ctrl && !alt => consumed(self.insert_char(c)),
            _ => EditOutcome::Passthrough,
        }
    }
}

fn consumed(changed: bool) -> EditOutcome {
    EditOutcome::Consumed { changed }
}

fn prev_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.saturating_sub(1);
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn next_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx + 1;
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i.min(s.len())
}

fn is_wordy(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Walk left from `cursor`, skipping a run of non-wordy chars and then
/// a run of wordy chars (readline / emacs convention). Returns the
/// byte index where the cursor should land.
fn word_boundary_left(s: &str, cursor: usize) -> usize {
    let mut idx = cursor;
    let chars: Vec<(usize, char)> = s[..cursor].char_indices().collect();
    let mut i = chars.len();
    while i > 0 && !is_wordy(chars[i - 1].1) {
        i -= 1;
        idx = chars[i].0;
    }
    while i > 0 && is_wordy(chars[i - 1].1) {
        i -= 1;
        idx = chars[i].0;
    }
    idx
}

/// Walk right from `cursor`, skipping a run of non-wordy chars and
/// then a run of wordy chars. Mirror of [`word_boundary_left`].
fn word_boundary_right(s: &str, cursor: usize) -> usize {
    let mut iter = s[cursor..].char_indices().peekable();
    while let Some(&(_, c)) = iter.peek() {
        if is_wordy(c) {
            break;
        }
        iter.next();
    }
    while let Some(&(_, c)) = iter.peek() {
        if !is_wordy(c) {
            break;
        }
        iter.next();
    }
    match iter.peek() {
        Some(&(off, _)) => cursor + off,
        None => s.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: mods,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn ctrl(c: char) -> KeyEvent {
        key(KeyCode::Char(c), KeyModifiers::CONTROL)
    }
    fn alt(c: char) -> KeyEvent {
        key(KeyCode::Char(c), KeyModifiers::ALT)
    }
    fn plain(code: KeyCode) -> KeyEvent {
        key(code, KeyModifiers::NONE)
    }

    fn from(s: &str, cursor: usize) -> TextInput {
        let mut t = TextInput::from_str(s);
        t.cursor = cursor;
        assert!(t.buf.is_char_boundary(t.cursor));
        t
    }

    #[test]
    fn insert_at_start_middle_end() {
        let mut t = TextInput::new();
        t.insert_char('a');
        t.insert_char('c');
        t.cursor = 1;
        t.insert_char('b');
        assert_eq!(t.as_str(), "abc");
        assert_eq!(t.cursor_byte(), 2);
    }

    #[test]
    fn backspace_at_zero_noop() {
        let mut t = TextInput::from_str("hi");
        t.cursor = 0;
        assert!(!t.backspace());
        assert_eq!(t.as_str(), "hi");
    }

    #[test]
    fn delete_forward_at_end_noop() {
        let mut t = TextInput::from_str("hi");
        assert!(!t.delete_forward());
        assert_eq!(t.as_str(), "hi");
    }

    #[test]
    fn cursor_moves_clamp() {
        let mut t = TextInput::from_str("ab");
        assert!(!t.move_right());
        assert!(t.move_left());
        assert!(t.move_left());
        assert!(!t.move_left());
        assert_eq!(t.cursor_byte(), 0);
    }

    #[test]
    fn word_ops_path_like() {
        // `/home/user_name/foo.bar`, cursor at end.
        let mut t = TextInput::from_str("/home/user_name/foo.bar");
        // Ctrl-W eats `bar` (the trailing wordy run, after skipping the `.`).
        t.delete_word_back();
        assert_eq!(t.as_str(), "/home/user_name/foo.");
        // Again: skip the trailing `.`, eat `foo`.
        t.delete_word_back();
        assert_eq!(t.as_str(), "/home/user_name/");
        // Again: skip `/`, eat `user_name` (underscore is wordy).
        t.delete_word_back();
        assert_eq!(t.as_str(), "/home/");
    }

    #[test]
    fn word_move_left_right_symmetric() {
        let mut t = TextInput::from_str("foo bar baz");
        t.move_home();
        assert_eq!(t.cursor_byte(), 0);
        t.move_word_right();
        assert_eq!(&t.as_str()[..t.cursor_byte()], "foo");
        t.move_word_right();
        assert_eq!(&t.as_str()[..t.cursor_byte()], "foo bar");
        t.move_word_left();
        assert_eq!(&t.as_str()[..t.cursor_byte()], "foo ");
    }

    #[test]
    fn ctrl_u_and_k() {
        let mut t = from("hello world", 5);
        t.delete_to_start();
        assert_eq!(t.as_str(), " world");
        assert_eq!(t.cursor_byte(), 0);
        let mut t = from("hello world", 5);
        t.delete_to_end();
        assert_eq!(t.as_str(), "hello");
        assert_eq!(t.cursor_byte(), 5);
    }

    #[test]
    fn multibyte_safe() {
        // `café` — `é` is 2 bytes.
        let mut t = TextInput::from_str("café");
        t.move_word_left();
        assert_eq!(t.cursor_byte(), 0);
        t.delete_forward(); // c
        assert_eq!(t.as_str(), "afé");
        t.move_end();
        t.backspace(); // é
        assert_eq!(t.as_str(), "af");
        // Emoji (4 bytes).
        let mut t = TextInput::from_str("a😀b");
        t.cursor = 1; // between `a` and emoji
        t.delete_forward();
        assert_eq!(t.as_str(), "ab");
    }

    #[test]
    fn set_and_clear() {
        let mut t = TextInput::from_str("xyz");
        t.set("abcd".into());
        assert_eq!(t.cursor_byte(), 4);
        t.clear();
        assert!(t.is_empty());
        assert_eq!(t.cursor_byte(), 0);
    }

    #[test]
    fn dispatcher_routes() {
        let mut t = TextInput::new();
        assert!(matches!(
            t.handle_edit_key(plain(KeyCode::Char('a'))),
            EditOutcome::Consumed { changed: true }
        ));
        assert!(matches!(
            t.handle_edit_key(ctrl('a')),
            EditOutcome::Consumed { changed: true }
        ));
        assert_eq!(t.cursor_byte(), 0);
        assert!(matches!(
            t.handle_edit_key(ctrl('e')),
            EditOutcome::Consumed { changed: true }
        ));
        assert_eq!(t.cursor_byte(), 1);
        // Enter is passthrough.
        assert_eq!(
            t.handle_edit_key(plain(KeyCode::Enter)),
            EditOutcome::Passthrough
        );
        // Ctrl-C is passthrough (caller handles it).
        assert_eq!(
            t.handle_edit_key(ctrl('c')),
            EditOutcome::Passthrough
        );
        // Alt-b is consumed (word left), changed=false at start.
        let _ = t.handle_edit_key(alt('b'));
    }
}
