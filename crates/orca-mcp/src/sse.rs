//! Reads a server-sent event stream as the HTML standard defines it: a line
//! ends in CRLF, CR or LF, a blank line ends an event, a line that begins with
//! a colon is a comment, and a field's value is what follows its colon, less
//! one space. Everything in this crate that reads an event stream, the answers
//! to streamable HTTP requests and the legacy SSE transport, reads it with
//! [`SseDecoder`].

/// One event of a stream.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct SseEvent {
    /// The event's `event:` field, when it has one.
    pub(crate) event: Option<String>,
    /// The event's `id:` field, when it has one.
    pub(crate) id: Option<String>,
    /// Every `data:` line, joined with `\n`.
    pub(crate) data: String,
}

/// Cuts the bytes of a stream into events as they come in, however the reads
/// that bring them break up lines and events. It limits no size: the caller
/// bounds what it reads, and what is held for an event
/// ([`SseDecoder::buffered_len`]).
#[derive(Debug, Default)]
pub(crate) struct SseDecoder {
    /// The line being read, up to its end.
    line: Vec<u8>,
    /// The last byte read was a CR: an LF that follows it belongs to the same
    /// line end, and ends no line of its own.
    after_cr: bool,
    /// What the event being read has so far. An event is returned only if it
    /// has a `data` line, so `data` stays `None` until one is read.
    event: Option<String>,
    id: Option<String>,
    data: Option<String>,
}

impl SseDecoder {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Takes the next bytes of the stream; returns the events they end, in
    /// order. A line that is not UTF-8 is an error. The decoder is of no more
    /// use then, and the events the bytes ended before that line are lost; a
    /// caller that reads on starts again with a new decoder.
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Result<Vec<SseEvent>, String> {
        let mut events = Vec::new();
        let mut rest = bytes;
        while let Some((&first, tail)) = rest.split_first() {
            if std::mem::take(&mut self.after_cr) && first == b'\n' {
                rest = tail;
                continue;
            }
            match rest.iter().position(|&byte| byte == b'\n' || byte == b'\r') {
                Some(end) => {
                    self.line.extend_from_slice(&rest[..end]);
                    self.after_cr = rest[end] == b'\r';
                    rest = &rest[end + 1..];
                    events.extend(self.end_line()?);
                }
                None => {
                    self.line.extend_from_slice(rest);
                    break;
                }
            }
        }
        Ok(events)
    }

    /// The event the stream ended inside, without the blank line that ends it.
    pub(crate) fn finish(mut self) -> Result<Option<SseEvent>, String> {
        // The last line may lack its end. It is read as any other, and being
        // no blank line, it ends no event.
        if !self.line.is_empty() {
            self.end_line()?;
        }
        Ok(self.end_event())
    }

    /// How many bytes the decoder holds for the event it is in the middle of,
    /// the line being read included. Comments and field names are not held,
    /// so they are not counted.
    pub(crate) fn buffered_len(&self) -> usize {
        self.line.len()
            + [&self.event, &self.id, &self.data]
                .into_iter()
                .flatten()
                .map(String::len)
                .sum::<usize>()
    }

    /// Acts on the line just read: a blank line ends the event, a comment is
    /// skipped, and a field sets what the event has.
    fn end_line(&mut self) -> Result<Option<SseEvent>, String> {
        let bytes = std::mem::take(&mut self.line);
        let line = std::str::from_utf8(&bytes)
            .map_err(|error| format!("an event stream line is not UTF-8: {error}"))?;
        if line.is_empty() {
            return Ok(self.end_event());
        }
        if line.starts_with(':') {
            return Ok(None);
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => self.event = Some(value.to_string()),
            "id" => self.id = Some(value.to_string()),
            "data" => match &mut self.data {
                Some(data) => {
                    data.push('\n');
                    data.push_str(value);
                }
                None => self.data = Some(value.to_string()),
            },
            // `retry`, and any field the standard does not name.
            _ => {}
        }
        Ok(None)
    }

    /// Ends the event being read, and returns it when it has data.
    fn end_event(&mut self) -> Option<SseEvent> {
        let event = self.event.take();
        let id = self.id.take();
        let data = self.data.take()?;
        Some(SseEvent { event, id, data })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An event that has `text` for its data and nothing else.
    fn data(text: &str) -> SseEvent {
        SseEvent {
            data: text.to_string(),
            ..SseEvent::default()
        }
    }

    #[test]
    fn an_event_split_across_reads_comes_out_whole() {
        let mut decoder = SseDecoder::new();

        assert_eq!(decoder.push(b"data: {\"a\""), Ok(Vec::new()));
        assert_eq!(decoder.push(b":1}\n\n"), Ok(vec![data("{\"a\":1}")]));
    }

    #[test]
    fn crlf_and_cr_line_ends_end_events() {
        let mut decoder = SseDecoder::new();

        assert_eq!(
            decoder.push(b"data: x\r\n\r\ndata: y\r\rdata: z\n\n"),
            Ok(vec![data("x"), data("y"), data("z")])
        );
    }

    #[test]
    fn a_crlf_split_between_reads_is_one_line_end() {
        let mut decoder = SseDecoder::new();

        assert_eq!(decoder.push(b"data: x\r"), Ok(Vec::new()));
        assert_eq!(decoder.push(b"\n\r\n"), Ok(vec![data("x")]));
        assert_eq!(decoder.push(b"data: y\n\n"), Ok(vec![data("y")]));

        // The LF that opens a read ends no line of its own. Were it to end
        // one, it would be a blank line, which ends the event after `a`, and
        // `b` would be an event of its own.
        assert_eq!(decoder.push(b"data: a\r"), Ok(Vec::new()));
        assert_eq!(decoder.push(b"\ndata: b\r\n\r\n"), Ok(vec![data("a\nb")]));
    }

    #[test]
    fn comment_lines_are_skipped() {
        let mut decoder = SseDecoder::new();

        assert_eq!(
            decoder.push(b": keepalive\n\ndata: x\n\n"),
            Ok(vec![data("x")])
        );
    }

    #[test]
    fn data_lines_are_joined_with_newlines() {
        let mut decoder = SseDecoder::new();

        assert_eq!(
            decoder.push(b"data: a\ndata: b\n\n"),
            Ok(vec![data("a\nb")])
        );
    }

    #[test]
    fn event_and_id_fields_are_kept() {
        let mut decoder = SseDecoder::new();

        assert_eq!(
            decoder.push(b"event: endpoint\nid: 7\ndata: /messages\n\n"),
            Ok(vec![SseEvent {
                event: Some("endpoint".to_string()),
                id: Some("7".to_string()),
                data: "/messages".to_string(),
            }])
        );
    }

    #[test]
    fn an_event_without_data_is_not_returned() {
        let mut decoder = SseDecoder::new();

        assert_eq!(decoder.push(b"event: ping\n\n"), Ok(Vec::new()));
    }

    #[test]
    fn finish_returns_an_unterminated_event() {
        let mut decoder = SseDecoder::new();

        assert_eq!(decoder.push(b"data: x"), Ok(Vec::new()));
        assert_eq!(decoder.finish(), Ok(Some(data("x"))));
    }
}
