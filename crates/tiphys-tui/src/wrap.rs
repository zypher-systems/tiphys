//! Breaking text into lines that fit.
//!
//! The conversation is drawn line by line so that it can be scrolled by a
//! known number of lines. Wrapping is done here, by display width, so a line
//! of wide characters or a long path without spaces still fits the screen.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Wraps `text` to `width` columns. Lines break at spaces where they can and
/// in the middle of a word where they must. Line breaks in the text are kept,
/// and so is the indentation of each line.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let paragraph = paragraph.trim_end_matches('\r');
        if paragraph.width() <= width {
            lines.push(paragraph.to_string());
            continue;
        }
        let mut line = String::new();
        let mut line_width = 0;
        for word in paragraph.split_inclusive(' ') {
            let word_width = word.width();
            if line_width + word.trim_end().width() > width && !line.is_empty() {
                lines.push(std::mem::take(&mut line).trim_end().to_string());
                line_width = 0;
            }
            if word_width <= width {
                line.push_str(word);
                line_width += word_width;
                continue;
            }
            // A word longer than a whole line is cut where the line ends.
            for c in word.chars() {
                let w = c.width().unwrap_or(0);
                if line_width + w > width {
                    lines.push(std::mem::take(&mut line));
                    line_width = 0;
                }
                line.push(c);
                line_width += w;
            }
        }
        lines.push(line.trim_end().to_string());
    }
    lines
}

/// Cuts `text` to `width` columns, ending in `…` if anything was cut.
pub fn fit(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w + 1 > width {
            break;
        }
        out.push(c);
        used += w;
    }
    if width > 0 {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_wraps_at_spaces_and_keeps_its_own_line_breaks() {
        let cases: [(&str, usize, &[&str]); 8] = [
            ("short", 10, &["short"]),
            ("", 10, &[""]),
            ("one two three four", 9, &["one two", "three", "four"]),
            (
                "first\n\nsecond line here",
                11,
                &["first", "", "second line", "here"],
            ),
            ("  indented stays", 20, &["  indented stays"]),
            (
                "/a/very/long/path/with/no/spaces",
                10,
                &["/a/very/lo", "ng/path/wi", "th/no/spac", "es"],
            ),
            (
                "see /a/very/long/path ok",
                10,
                &["see", "/a/very/lo", "ng/path ok"],
            ),
            ("日本語のテキスト", 6, &["日本語", "のテキ", "スト"]),
        ];
        for (text, width, expected) in cases {
            assert_eq!(wrap(text, width), expected, "{text:?} at {width}");
        }
    }

    #[test]
    fn every_wrapped_line_fits_and_nothing_but_spaces_is_lost() {
        let text = "The quick brown fox jumps over the lazy dog, /usr/local/bin/tiphys --version, and 日本語 too.";
        for width in 1..40 {
            let lines = wrap(text, width);
            assert!(
                lines.iter().all(|line| line.width() <= width.max(2)),
                "width {width}: {lines:?}"
            );
            let squeezed = |s: &str| s.chars().filter(|c| *c != ' ').collect::<String>();
            assert_eq!(squeezed(&lines.concat()), squeezed(text), "width {width}");
        }
    }

    #[test]
    fn text_that_does_not_fit_is_cut_with_a_mark() {
        assert_eq!(fit("short", 10), "short");
        assert_eq!(fit("exactly ten", 11), "exactly ten");
        assert_eq!(fit("a long line of text", 8), "a long …");
        assert_eq!(fit("日本語のテキスト", 7), "日本語…");
        assert_eq!(fit("abc", 0), "");
    }
}
