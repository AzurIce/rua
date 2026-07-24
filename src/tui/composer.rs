use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Editable user draft with a cursor that is always on a grapheme boundary.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Composer {
    text: String,
    cursor: usize,
}

/// The horizontally visible portion of a composer and its cursor column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposerViewport {
    pub text: String,
    pub cursor_column: u16,
}

impl Composer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    pub fn insert_char(&mut self, ch: char) {
        self.text.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
    }

    pub fn insert_str(&mut self, text: &str) {
        self.text.insert_str(self.cursor, text);
        self.cursor += text.len();
    }

    pub fn move_left(&mut self) {
        self.cursor = previous_grapheme_boundary(&self.text, self.cursor);
    }

    pub fn move_right(&mut self) {
        self.cursor = next_grapheme_boundary(&self.text, self.cursor);
    }

    pub fn move_to_start(&mut self) {
        self.cursor = 0;
    }

    pub fn move_to_end(&mut self) {
        self.cursor = self.text.len();
    }

    pub fn delete_backward(&mut self) {
        let previous = previous_grapheme_boundary(&self.text, self.cursor);
        if previous != self.cursor {
            self.text.replace_range(previous..self.cursor, "");
            self.cursor = previous;
        }
    }

    pub fn delete_forward(&mut self) {
        let next = next_grapheme_boundary(&self.text, self.cursor);
        if next != self.cursor {
            self.text.replace_range(self.cursor..next, "");
        }
    }

    /// Return a grapheme-safe horizontal viewport that always leaves a cell
    /// for the hardware cursor.
    pub fn viewport(&self, width: u16) -> ComposerViewport {
        if width == 0 {
            return ComposerViewport {
                text: String::new(),
                cursor_column: 0,
            };
        }

        let prefix_budget = usize::from(width.saturating_sub(1));
        let line_start = self.text[..self.cursor]
            .rfind('\n')
            .map_or(0, |index| index + 1);
        let line_end = self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |index| self.cursor + index);
        let mut start = self.cursor;
        let mut prefix_width = 0;

        for (relative_index, grapheme) in self.text[line_start..self.cursor]
            .grapheme_indices(true)
            .rev()
        {
            let grapheme_width = grapheme.width();
            if prefix_width + grapheme_width > prefix_budget {
                break;
            }
            start = line_start + relative_index;
            prefix_width += grapheme_width;
        }

        let mut visible = String::new();
        let mut visible_width = 0;
        for grapheme in self.text[start..line_end].graphemes(true) {
            let grapheme_width = grapheme.width();
            if visible_width + grapheme_width > usize::from(width) {
                break;
            }
            visible.push_str(grapheme);
            visible_width += grapheme_width;
        }

        ComposerViewport {
            text: visible,
            cursor_column: prefix_width as u16,
        }
    }
}

fn previous_grapheme_boundary(text: &str, cursor: usize) -> usize {
    text[..cursor]
        .grapheme_indices(true)
        .next_back()
        .map_or(0, |(index, _)| index)
}

fn next_grapheme_boundary(text: &str, cursor: usize) -> usize {
    text[cursor..]
        .grapheme_indices(true)
        .nth(1)
        .map_or(text.len(), |(index, _)| cursor + index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn movement_and_deletion_use_grapheme_boundaries() {
        let mut composer = Composer::new();
        composer.insert_str("a👨‍👩‍👧‍👦你");

        composer.move_left();
        assert_eq!(composer.cursor(), "a👨‍👩‍👧‍👦".len());
        composer.delete_backward();

        assert_eq!(composer.text(), "a你");
        assert_eq!(composer.cursor(), 1);
        composer.delete_forward();
        assert_eq!(composer.text(), "a");
    }

    #[test]
    fn viewport_counts_cjk_display_columns_and_keeps_cursor_visible() {
        let mut composer = Composer::new();
        composer.insert_str("ab中文cd");

        let viewport = composer.viewport(5);

        assert_eq!(viewport.text, "文cd");
        assert_eq!(viewport.cursor_column, 4);
    }

    #[test]
    fn insertion_at_a_grapheme_boundary_preserves_unicode() {
        let mut composer = Composer::new();
        composer.insert_str("你好");
        composer.move_left();
        composer.insert_char('，');

        assert_eq!(composer.text(), "你，好");
    }

    #[test]
    fn multiline_paste_is_preserved_while_viewport_shows_the_cursor_line() {
        let mut composer = Composer::new();
        composer.insert_str("first\n第二行");

        let viewport = composer.viewport(10);

        assert_eq!(composer.text(), "first\n第二行");
        assert_eq!(viewport.text, "第二行");
        assert_eq!(viewport.cursor_column, 6);
    }
}
