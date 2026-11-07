//! A single-line text value with a movable cursor.

use ratatui::crossterm::event::{KeyCode, KeyEvent};

#[derive(Clone, Debug, Default)]
pub struct TextInput {
    text: String,
    /// Cursor position in chars, `0..=len`.
    cursor: usize,
}

impl TextInput {
    /// Starts with the cursor at the end.
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        let cursor = text.chars().count();
        Self { text, cursor }
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn len(&self) -> usize {
        self.text.chars().count()
    }

    pub fn set_cursor(&mut self, cursor: usize) {
        self.cursor = cursor.min(self.len());
    }

    pub fn insert(&mut self, c: char) {
        let at = self.byte_index(self.cursor);
        self.text.insert(at, c);
        self.cursor += 1;
    }

    /// Empties the input and returns what it held.
    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }

    /// Handles cursor movement and deletion keys; returns whether `key` was used.
    /// Typing characters is left to the caller, since fields filter differently.
    pub fn edit(&mut self, key: &KeyEvent) -> bool {
        match key.code {
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.set_cursor(self.cursor + 1),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.len(),
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                self.text.remove(self.byte_index(self.cursor));
            }
            KeyCode::Delete if self.cursor < self.len() => {
                self.text.remove(self.byte_index(self.cursor));
            }
            KeyCode::Backspace | KeyCode::Delete => {}
            _ => return false,
        }
        true
    }

    /// First char to show in a field `width` cells wide so the cursor (which
    /// may sit one past the last char) stays visible.
    pub fn scroll(&self, width: usize) -> usize {
        if width == 0 {
            return self.cursor;
        }
        self.cursor.saturating_sub(width - 1)
    }

    fn byte_index(&self, chars: usize) -> usize {
        self.text
            .char_indices()
            .nth(chars)
            .map_or(self.text.len(), |(i, _)| i)
    }
}

impl From<&str> for TextInput {
    fn from(text: &str) -> Self {
        Self::new(text)
    }
}

impl From<String> for TextInput {
    fn from(text: String) -> Self {
        Self::new(text)
    }
}

impl PartialEq<&str> for TextInput {
    fn eq(&self, other: &&str) -> bool {
        self.text == *other
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyModifiers;

    fn press(input: &mut TextInput, code: KeyCode) -> bool {
        input.edit(&KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn starts_with_cursor_at_end() {
        let input = TextInput::new("abc");
        assert_eq!(input.cursor(), 3);
        assert_eq!(input, "abc");
    }

    #[test]
    fn inserts_at_the_cursor() {
        let mut input = TextInput::new("0.0.0.0");
        input.set_cursor(2);
        press(&mut input, KeyCode::Delete);
        input.insert('1');
        input.insert('2');
        assert_eq!(input, "0.12.0.0");
        assert_eq!(input.cursor(), 4);
    }

    #[test]
    fn movement_is_clamped() {
        let mut input = TextInput::new("ab");
        press(&mut input, KeyCode::Right);
        assert_eq!(input.cursor(), 2);
        press(&mut input, KeyCode::Home);
        press(&mut input, KeyCode::Left);
        assert_eq!(input.cursor(), 0);
        press(&mut input, KeyCode::End);
        assert_eq!(input.cursor(), 2);
        input.set_cursor(99);
        assert_eq!(input.cursor(), 2);
    }

    #[test]
    fn backspace_and_delete_at_the_edges_do_nothing() {
        let mut input = TextInput::new("ab");
        assert!(press(&mut input, KeyCode::Delete));
        input.set_cursor(0);
        assert!(press(&mut input, KeyCode::Backspace));
        assert_eq!(input, "ab");
        press(&mut input, KeyCode::Delete);
        assert_eq!(input, "b");
    }

    #[test]
    fn handles_multibyte_chars() {
        let mut input = TextInput::new("héllo");
        input.set_cursor(2);
        press(&mut input, KeyCode::Backspace);
        input.insert('e');
        assert_eq!(input, "hello");
    }

    #[test]
    fn other_keys_are_left_to_the_caller() {
        let mut input = TextInput::new("ab");
        assert!(!press(&mut input, KeyCode::Char('x')));
        assert!(!press(&mut input, KeyCode::Enter));
        assert_eq!(input, "ab");
    }

    #[test]
    fn take_empties_the_input() {
        let mut input = TextInput::new("hi");
        assert_eq!(input.take(), "hi");
        assert_eq!(input, "");
        assert_eq!(input.cursor(), 0);
    }

    #[test]
    fn scroll_keeps_the_cursor_in_view() {
        let mut input = TextInput::new("abcdefghij");
        assert_eq!(
            input.scroll(4),
            7,
            "cursor past the end shows the last 3 chars"
        );
        input.set_cursor(2);
        assert_eq!(input.scroll(4), 0);
        assert_eq!(input.scroll(20), 0);
    }
}
