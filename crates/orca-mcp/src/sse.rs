//! Reads a server-sent event stream as the HTML standard defines it: a line
//! ends in CRLF, CR or LF, a blank line ends an event, a line that begins with
//! a colon is a comment, and a field's value is what follows its colon, less
//! one space. Everything in this crate that reads an event stream, the answers
//! to streamable HTTP requests and the legacy SSE transport, reads it with
//! [`SseDecoder`].

use std::collections::VecDeque;

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
/// that bring them break up lines and events: what it gives out in all, and in
/// which order, does not depend on where the reads end. It limits no size: the
/// caller bounds what it reads, and what is held for an event
/// ([`SseDecoder::buffered_len`]).
///
/// An event with a line that is not UTF-8 cannot be read. It is dropped whole,
/// up to its blank line, and given out as an error in its place, after the
/// events before it. The decoder reads on after it.
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
    /// Why the event being read cannot be read, once one of its lines is not
    /// UTF-8. The event is dropped when it ends.
    unreadable: Option<String>,
    /// What the bytes read so far have ended and was not given out yet, in
    /// the order of the stream: each event, and each unreadable one as an
    /// error.
    ready: VecDeque<Result<SseEvent, String>>,
}

impl SseDecoder {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Takes the next bytes of the stream; returns the events they end, in
    /// order, up to an event that cannot be read. That one is the error of the
    /// call that follows, so that no event is lost to it. Then the decoder
    /// goes on with what follows, and is good for more bytes.
    ///
    /// A call may leave more to give out, after an error, or after the
    /// events before one. Ask again, with no bytes, until it returns no
    /// events.
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Result<Vec<SseEvent>, String> {
        self.cut(bytes);
        let mut events = Vec::new();
        while let Some(item) = self.ready.pop_front() {
            match item {
                Ok(event) => events.push(event),
                Err(error) if events.is_empty() => return Err(error),
                Err(error) => {
                    self.ready.push_front(Err(error));
                    break;
                }
            }
        }
        Ok(events)
    }

    /// The event the stream ended inside, without the blank line that ends it,
    /// or why it cannot be read. For a decoder that `push` has given out
    /// everything of.
    pub(crate) fn finish(mut self) -> Result<Option<SseEvent>, String> {
        debug_assert!(
            self.ready.is_empty(),
            "finish is for a decoder that has given out everything"
        );
        // The last line may lack its end. It is read as any other, and being
        // no blank line, it ends no event.
        if !self.line.is_empty() {
            self.end_line();
        }
        self.end_event();
        self.ready.pop_front().transpose()
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

    /// Cuts `bytes` into lines, which it acts on, one after the other.
    fn cut(&mut self, bytes: &[u8]) {
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
                    self.end_line();
                }
                None => {
                    self.line.extend_from_slice(rest);
                    break;
                }
            }
        }
    }

    /// Acts on the line just read: a blank line ends the event, a comment is
    /// skipped, and a field sets what the event has. A line that is not UTF-8
    /// makes the event unreadable: what it has is dropped, and so are its
    /// other lines.
    fn end_line(&mut self) {
        let bytes = std::mem::take(&mut self.line);
        let line = match std::str::from_utf8(&bytes) {
            Ok(line) => line,
            Err(error) => {
                if self.unreadable.is_none() {
                    self.unreadable = Some(format!("an event stream line is not UTF-8: {error}"));
                }
                self.event = None;
                self.id = None;
                self.data = None;
                return;
            }
        };
        if line.is_empty() {
            self.end_event();
            return;
        }
        if self.unreadable.is_some() || line.starts_with(':') {
            return;
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
    }

    /// Ends the event being read: it is given out when it has data, or as an
    /// error when it cannot be read.
    fn end_event(&mut self) {
        let event = self.event.take();
        let id = self.id.take();
        let data = self.data.take();
        if let Some(error) = self.unreadable.take() {
            self.ready.push_back(Err(error));
        } else if let Some(data) = data {
            self.ready.push_back(Ok(SseEvent { event, id, data }));
        }
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

    /// Everything a decoder gives out for `reads`, in the order of the
    /// stream: each event, and each unreadable one as an `Err`. After each
    /// read it asks again, with no bytes, until there is no more, as the
    /// readers of a stream do.
    fn read_all(reads: &[&[u8]]) -> Vec<Result<SseEvent, String>> {
        let mut decoder = SseDecoder::new();
        let mut given = Vec::new();
        for read in reads {
            let mut bytes = *read;
            loop {
                match decoder.push(bytes) {
                    Ok(events) if events.is_empty() => break,
                    Ok(events) => given.extend(events.into_iter().map(Ok)),
                    Err(error) => given.push(Err(error)),
                }
                bytes = b"";
            }
        }
        given
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

    #[test]
    fn an_event_before_an_unreadable_line_comes_out_and_the_error_follows() {
        let mut decoder = SseDecoder::new();

        // One read: an event, then one that has a line that is not UTF-8.
        assert_eq!(
            decoder.push(b"data: ok\n\ndata: \xff\n\n"),
            Ok(vec![data("ok")])
        );
        let error = decoder
            .push(b"")
            .expect_err("the unreadable event is reported by the next call");
        assert!(error.contains("not UTF-8"), "{error}");
        assert_eq!(decoder.push(b""), Ok(Vec::new()));
        assert_eq!(decoder.push(b"data: next\n\n"), Ok(vec![data("next")]));
    }

    #[test]
    fn an_unreadable_event_is_dropped_whole_and_the_decoder_reads_on() {
        let mut decoder = SseDecoder::new();

        // `a`, then an event whose second line is not UTF-8 (its valid lines
        // go with it), then `b`.
        assert_eq!(
            decoder.push(b"data: a\n\ndata: x\n\xff\ndata: y\n\ndata: b\n\n"),
            Ok(vec![data("a")])
        );
        assert!(decoder.push(b"").is_err());
        assert_eq!(decoder.push(b""), Ok(vec![data("b")]));
        assert_eq!(decoder.push(b""), Ok(Vec::new()));
    }

    #[test]
    fn a_comment_that_is_not_utf_8_is_an_error_too() {
        let mut decoder = SseDecoder::new();

        assert!(decoder.push(b": \xff\n\ndata: x\n\n").is_err());
        assert_eq!(decoder.push(b""), Ok(vec![data("x")]));
    }

    #[test]
    fn the_events_do_not_depend_on_where_reads_are_cut() {
        // Three line ends, a comment, a character of two bytes, and two
        // events that have a line that is not UTF-8.
        let stream: &[u8] = b"event: a\r\ndata: caf\xc3\xa9\r\n\r\n: keepalive\rdata: \xff\r\n\r\n\
data: 2\ndata: 3\n\ndata: x\n\xc3\n\ndata: 4\r\r";
        let whole = read_all(&[stream]);
        let events: Vec<_> = whole
            .iter()
            .flatten()
            .map(|event| event.data.as_str())
            .collect();
        assert_eq!(events, ["café", "2\n3", "4"]);
        assert_eq!(whole.iter().filter(|item| item.is_err()).count(), 2);

        for cut in 0..=stream.len() {
            assert_eq!(
                read_all(&[&stream[..cut], &stream[cut..]]),
                whole,
                "cut after {cut} bytes"
            );
        }
        assert_eq!(read_all(&stream.chunks(1).collect::<Vec<_>>()), whole);
    }

    #[test]
    fn finish_reports_an_unreadable_event_the_stream_ended_inside() {
        let mut decoder = SseDecoder::new();

        assert_eq!(decoder.push(b"data: x\n\xff\ndata: y\n"), Ok(Vec::new()));
        let error = decoder.finish().expect_err("the event is not UTF-8");
        assert!(error.contains("not UTF-8"), "{error}");
    }
}
