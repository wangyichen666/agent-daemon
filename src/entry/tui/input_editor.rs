use unicode_width::UnicodeWidthChar;

/// 面向终端输入框的字符编辑器。光标以 Unicode 标量位置而非字节位置保存，
/// 因此删除或移动不会截断 CJK/emoji 字符。
#[derive(Default)]
pub(super) struct InputEditor {
    buffer: Vec<char>,
    cursor: usize,
}

impl InputEditor {
    pub(super) fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    pub(super) fn is_blank(&self) -> bool {
        self.buffer
            .iter()
            .all(|character| character.is_whitespace())
    }

    pub(super) fn is_single_line(&self) -> bool {
        !self.buffer.contains(&'\n')
    }

    pub(super) fn text(&self) -> String {
        self.buffer.iter().collect()
    }

    pub(super) fn clear(&mut self) {
        self.buffer.clear();
        self.cursor = 0;
    }

    pub(super) fn replace(&mut self, text: impl AsRef<str>) {
        self.buffer = text.as_ref().chars().collect();
        self.cursor = self.buffer.len();
    }

    pub(super) fn take(&mut self) -> String {
        let text = self.text();
        self.clear();
        text
    }

    pub(super) fn insert(&mut self, character: char) {
        self.buffer.insert(self.cursor, character);
        self.cursor += 1;
    }

    pub(super) fn insert_text(&mut self, text: &str) {
        for character in text.chars().filter(|character| *character != '\r') {
            self.insert(character);
        }
    }

    pub(super) fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.buffer.remove(self.cursor);
        }
    }

    pub(super) fn delete(&mut self) {
        if self.cursor < self.buffer.len() {
            self.buffer.remove(self.cursor);
        }
    }

    pub(super) fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub(super) fn move_right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.buffer.len());
    }

    pub(super) fn move_line_start(&mut self) {
        while self.cursor > 0 && self.buffer[self.cursor - 1] != '\n' {
            self.cursor -= 1;
        }
    }

    pub(super) fn move_line_end(&mut self) {
        while self.cursor < self.buffer.len() && self.buffer[self.cursor] != '\n' {
            self.cursor += 1;
        }
    }

    pub(super) fn move_word_left(&mut self) {
        while self.cursor > 0 && self.buffer[self.cursor - 1].is_whitespace() {
            self.cursor -= 1;
        }
        while self.cursor > 0 && !self.buffer[self.cursor - 1].is_whitespace() {
            self.cursor -= 1;
        }
    }

    pub(super) fn move_word_right(&mut self) {
        while self.cursor < self.buffer.len() && self.buffer[self.cursor].is_whitespace() {
            self.cursor += 1;
        }
        while self.cursor < self.buffer.len() && !self.buffer[self.cursor].is_whitespace() {
            self.cursor += 1;
        }
    }

    pub(super) fn delete_word_left(&mut self) {
        let end = self.cursor;
        self.move_word_left();
        self.buffer.drain(self.cursor..end);
    }

    /// 返回在给定显示宽度下，光标所在的视觉行和列。宽字符按终端单元格计数。
    pub(super) fn visual_cursor(&self, width: u16) -> (usize, usize) {
        let width = usize::from(width.max(1));
        let mut row = 0;
        let mut column = 0;
        for character in self.buffer.iter().take(self.cursor) {
            if *character == '\n' {
                row += 1;
                column = 0;
                continue;
            }
            let character_width = character.width().unwrap_or(0);
            if character_width > 0 && column + character_width > width {
                row += 1;
                column = 0;
            }
            column += character_width;
            if column >= width {
                row += 1;
                column = 0;
            }
        }
        (row, column)
    }
}

#[cfg(test)]
mod tests {
    use super::InputEditor;

    #[test]
    fn edits_cjk_without_using_byte_offsets() {
        let mut editor = InputEditor::default();
        editor.insert_text("你好 world");
        editor.delete_word_left();
        assert_eq!(editor.text(), "你好 ");
        editor.backspace();
        assert_eq!(editor.text(), "你好");
        editor.move_left();
        editor.backspace();
        assert_eq!(editor.text(), "好");
    }

    #[test]
    fn computes_wrapped_visual_cursor_for_wide_characters() {
        let mut editor = InputEditor::default();
        editor.insert_text("ab中文");
        assert_eq!(editor.visual_cursor(4), (1, 2));
        editor.insert('\n');
        editor.insert_text("x");
        assert_eq!(editor.visual_cursor(4), (2, 1));
    }
}
