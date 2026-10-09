//! Server-sent events, decoded as the bytes arrive.
//!
//! A reply is streamed as events separated by a blank line, each with one or
//! more `data:` lines. Network chunks split anywhere: in the middle of an
//! event, of a line, or of a single character. The decoder holds what is
//! incomplete and hands back the data of every event that is whole.

/// Turns chunks of a response body into the `data` of its events.
#[derive(Debug, Default)]
pub(crate) struct SseDecoder {
    /// Bytes at the end of the last chunk that are part of a character.
    partial: Vec<u8>,
    /// Text not yet ended by a blank line.
    text: String,
}

impl SseDecoder {
    /// Adds a chunk. Returns the data of each event it completed, in order.
    /// Comments and events with no data, which providers send to keep a
    /// connection open, return nothing.
    pub(crate) fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.partial.extend_from_slice(chunk);
        let whole = whole_characters(&mut self.partial);
        self.text.push_str(&whole);
        // A `\r\n` can itself be split across chunks, so this looks at all the
        // text held, not just the new part.
        if self.text.contains('\r') {
            self.text = self.text.replace("\r\n", "\n");
        }
        let mut events = Vec::new();
        while let Some(end) = self.text.find("\n\n") {
            let block: String = self.text.drain(..end + 2).collect();
            events.extend(data_of(&block));
        }
        events
    }

    /// The body has ended. Returns the data of a last event that was not
    /// followed by a blank line, if there is one.
    pub(crate) fn finish(&mut self) -> Option<String> {
        let rest = String::from_utf8_lossy(&std::mem::take(&mut self.partial)).into_owned();
        self.text.push_str(&rest);
        data_of(&std::mem::take(&mut self.text))
    }
}

/// Takes the complete characters off the front of `raw`, leaving the start of
/// a character that the next chunk will finish. Decoding each chunk on its own
/// would turn a character split across two into replacement marks.
fn whole_characters(raw: &mut Vec<u8>) -> String {
    let valid = match std::str::from_utf8(raw) {
        Ok(_) => raw.len(),
        // Cut short at the end: the rest waits for the next chunk.
        Err(e) if e.error_len().is_none() => e.valid_up_to(),
        // Bytes that are not UTF-8 at all: there is nothing to wait for.
        Err(_) => raw.len(),
    };
    let tail = raw.split_off(valid);
    let text = String::from_utf8_lossy(raw).into_owned();
    *raw = tail;
    text
}

/// The `data` of one event: its `data:` lines joined by newlines.
fn data_of(block: &str) -> Option<String> {
    let mut data: Option<String> = None;
    for line in block.lines() {
        let Some(rest) = line.strip_prefix("data:") else {
            continue;
        };
        let rest = rest.strip_prefix(' ').unwrap_or(rest);
        match &mut data {
            Some(data) => {
                data.push('\n');
                data.push_str(rest);
            }
            None => data = Some(rest.to_string()),
        }
    }
    data.filter(|data| !data.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(chunks: &[&[u8]]) -> Vec<String> {
        let mut decoder = SseDecoder::default();
        let mut events = Vec::new();
        for chunk in chunks {
            events.extend(decoder.push(chunk));
        }
        events.extend(decoder.finish());
        events
    }

    #[test]
    fn events_come_out_whole_however_the_bytes_are_chunked() {
        let body = "data: {\"a\":1}\n\ndata: {\"b\":2}\n\ndata: [DONE]\n\n";
        let expected = ["{\"a\":1}", "{\"b\":2}", "[DONE]"];
        assert_eq!(decode(&[body.as_bytes()]), expected);
        // Every possible split into two chunks.
        for cut in 0..body.len() {
            let (first, second) = body.as_bytes().split_at(cut);
            assert_eq!(decode(&[first, second]), expected, "cut at {cut}");
        }
        // One byte at a time.
        let bytes: Vec<&[u8]> = body.as_bytes().chunks(1).collect();
        assert_eq!(decode(&bytes), expected);
    }

    #[test]
    fn a_character_split_across_chunks_arrives_whole() {
        let body = "data: naïve — 日本語\n\n".as_bytes();
        for cut in 0..body.len() {
            let (first, second) = body.split_at(cut);
            assert_eq!(decode(&[first, second]), ["naïve — 日本語"], "cut at {cut}");
        }
    }

    #[test]
    fn carriage_returns_comments_and_other_fields_are_handled() {
        let cases: [(&str, &[&str]); 6] = [
            ("data: a\r\n\r\ndata: b\r\n\r\n", &["a", "b"]),
            (": keep-alive\n\ndata: a\n\n", &["a"]),
            ("event: message\nid: 7\ndata: a\n\n", &["a"]),
            ("data: one\ndata: two\n\n", &["one\ntwo"]),
            ("data:no-space\n\n", &["no-space"]),
            (
                "data: last, with no blank line after",
                &["last, with no blank line after"],
            ),
        ];
        for (body, expected) in cases {
            assert_eq!(decode(&[body.as_bytes()]), expected, "{body:?}");
        }
        // A `\r\n` split between two chunks is still one line ending.
        assert_eq!(
            decode(&[b"data: a\r", b"\n\r", b"\ndata: b\r\n\r\n"]),
            ["a", "b"]
        );
    }

    #[test]
    fn keep_alives_alone_complete_no_event() {
        let mut decoder = SseDecoder::default();
        assert!(decoder.push(b": processing\n\n").is_empty());
        assert!(decoder.push(b"event: ping\n\n").is_empty());
        assert_eq!(decoder.finish(), None);
    }
}
