//! A one-line text field.
//!
//! Used for the message being typed, for the fields of the setup form, and
//! for the key. A masked field shows a dot for each character: what is typed
//! or pasted into it is never drawn.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// The character a masked field shows in place of each one typed.
const MASK: char = '•';

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Input {
    text: String,
    /// The cursor, as a byte offset on a character boundary.
    cursor: usize,
    masked: bool,
}

impl Input {
    pub fn new(text: &str) -> Self {
        Self {
            text: text.to_string(),
            cursor: text.len(),
            masked: false,
        }
    }

    /// A field whose contents are never drawn.
    pub fn masked() -> Self {
        Self {
            masked: true,
            ..Self::default()
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Empties the field and returns what was in it.
    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }

    /// Types or pastes text at the cursor. A field is one line, so line
    /// breaks and other control characters are left out: a key copied with
    /// its trailing newline goes in clean.
    pub fn insert(&mut self, text: &str) {
        for c in text.chars().filter(|c| !c.is_control()) {
            self.text.insert(self.cursor, c);
            self.cursor += c.len_utf8();
        }
    }

    pub fn backspace(&mut self) {
        if let Some(previous) = self.text[..self.cursor].chars().next_back() {
            self.cursor -= previous.len_utf8();
            self.text.remove(self.cursor);
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.text.len() {
            self.text.remove(self.cursor);
        }
    }

    pub fn left(&mut self) {
        if let Some(previous) = self.text[..self.cursor].chars().next_back() {
            self.cursor -= previous.len_utf8();
        }
    }

    pub fn right(&mut self) {
        if let Some(next) = self.text[self.cursor..].chars().next() {
            self.cursor += next.len_utf8();
        }
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.text.len();
    }

    /// Deletes back to the start of the word before the cursor.
    pub fn delete_word(&mut self) {
        let before = &self.text[..self.cursor];
        let trimmed = before.trim_end();
        let start = trimmed
            .char_indices()
            .rev()
            .find(|(_, c)| c.is_whitespace())
            .map_or(0, |(index, c)| index + c.len_utf8());
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    /// What to draw in a space `width` columns wide, and the column the
    /// cursor is at. Text longer than the space scrolls so the cursor stays
    /// in view.
    pub fn visible(&self, width: usize) -> (String, usize) {
        let shown: String = if self.masked {
            self.text.chars().map(|_| MASK).collect()
        } else {
            self.text.clone()
        };
        let before = if self.masked {
            self.text[..self.cursor].chars().count()
        } else {
            self.text[..self.cursor].width()
        };
        if width == 0 {
            return (String::new(), 0);
        }
        // Leave a column for the cursor itself when it sits past the end.
        let skip = (before + 1).saturating_sub(width);
        let mut out = String::new();
        let (mut column, mut used) = (0, 0);
        for c in shown.chars() {
            let w = c.width().unwrap_or(0);
            if column >= skip && used + w <= width {
                out.push(c);
                used += w;
            }
            column += w;
        }
        (out, before - skip)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_moving_and_deleting_edit_at_the_cursor() {
        let mut input = Input::default();
        input.insert("helo");
        input.left();
        input.insert("l");
        assert_eq!(input.text(), "hello");
        input.home();
        input.delete();
        assert_eq!(input.text(), "ello");
        input.end();
        input.backspace();
        assert_eq!(input.text(), "ell");
        input.left();
        input.right();
        input.right();
        input.insert("é日");
        input.backspace();
        assert_eq!(input.text(), "ellé");
        // Nothing to delete at either end.
        input.delete();
        input.home();
        input.backspace();
        assert_eq!(input.text(), "ellé");
        assert_eq!(input.take(), "ellé");
        assert!(input.is_empty());
    }

    #[test]
    fn a_pasted_key_goes_in_without_its_line_break() {
        let mut input = Input::masked();
        input.insert("sk-live-123\r\n");
        assert_eq!(input.text(), "sk-live-123");
    }

    #[test]
    fn a_masked_field_never_draws_what_it_holds() {
        let mut input = Input::masked();
        input.insert("sk-live-123");
        let (shown, cursor) = input.visible(40);
        assert_eq!(shown, "•••••••••••");
        assert_eq!(cursor, 11);
        assert!(!shown.contains("sk"));
    }

    #[test]
    fn long_text_scrolls_to_keep_the_cursor_in_view() {
        let mut input = Input::new("abcdefghij");
        assert_eq!(input.visible(20), ("abcdefghij".to_string(), 10));
        // At the end: the last characters, with a column left for the cursor.
        assert_eq!(input.visible(5), ("ghij".to_string(), 4));
        input.home();
        assert_eq!(input.visible(5), ("abcde".to_string(), 0));
        // Wide characters take two columns each.
        let wide = Input::new("日本語");
        assert_eq!(wide.visible(10), ("日本語".to_string(), 6));
        assert_eq!(Input::new("abc").visible(0), (String::new(), 0));
    }

    #[test]
    fn deleting_a_word_stops_at_the_space_before_it() {
        let cases = [
            ("how full is", "how full "),
            ("how full is  ", "how full "),
            ("word", ""),
            ("", ""),
        ];
        for (before, after) in cases {
            let mut input = Input::new(before);
            input.delete_word();
            assert_eq!(input.text(), after, "{before:?}");
        }
    }
}
