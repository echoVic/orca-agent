use std::io::{self, BufRead, Write};
use std::sync::{Arc, Mutex};

use crate::protocol::{
    DriverError, DriverLine, ErrorKind, Reply, Request, RequestLine, ResponseLine,
};

/// Writes complete, flushed driver lines; clones share the same output lock.
#[derive(Clone)]
pub struct LineWriter {
    output: Arc<Mutex<Box<dyn Write + Send>>>,
}

impl LineWriter {
    pub fn new(output: impl Write + Send + 'static) -> Self {
        Self {
            output: Arc::new(Mutex::new(Box::new(output))),
        }
    }

    pub fn send(&self, line: &DriverLine) -> io::Result<()> {
        let mut bytes = serde_json::to_vec(line)?;
        bytes.push(b'\n');
        let mut output = self
            .output
            .lock()
            .map_err(|_| io::Error::other("driver output lock poisoned"))?;
        output.write_all(&bytes)?;
        output.flush()
    }
}

pub trait DriverHandler {
    fn handle(&mut self, request: Request, events: &LineWriter) -> Result<Reply, DriverError>;
    /// Release held keys and buttons, remove the Esc listener, and drop the session lock.
    fn shutdown(&mut self);
}

/// Reads until EOF or shutdown. Cleanup also runs when either pipe fails.
pub fn serve(
    input: impl BufRead,
    output: LineWriter,
    handler: &mut dyn DriverHandler,
) -> io::Result<()> {
    let result = (|| {
        for line in input.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let request = match serde_json::from_str::<RequestLine>(&line) {
                Ok(request) => request,
                Err(error) => {
                    output.send(&DriverLine::Response(ResponseLine {
                        id: 0,
                        result: Err(DriverError {
                            kind: ErrorKind::Failed,
                            message: format!("Invalid request line: {error}"),
                            candidates: vec![],
                        }),
                    }))?;
                    continue;
                }
            };
            let shutdown = matches!(request.request, Request::Shutdown);
            let result = if shutdown {
                Ok(Reply::Done)
            } else {
                handler.handle(request.request, &output)
            };
            output.send(&DriverLine::Response(ResponseLine {
                id: request.id,
                result,
            }))?;
            if shutdown {
                break;
            }
        }
        Ok(())
    })();
    handler.shutdown();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeDesktop;
    use crate::protocol::*;
    use std::io::{self, Cursor, Write};
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);
    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn an_unparseable_line_gets_an_error_and_serving_continues() {
        let capture = Capture::default();
        let output = LineWriter::new(capture.clone());
        let mut handler = FakeDesktop::with_textedit();
        serve(
            Cursor::new("not json\n \n{\"id\":7,\"method\":\"list_apps\"}\n"),
            output,
            &mut handler,
        )
        .unwrap();
        let bytes = capture.0.lock().unwrap().clone();
        let lines: Vec<DriverLine> = String::from_utf8(bytes)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert!(matches!(
            &lines[0],
            DriverLine::Response(ResponseLine {
                id: 0,
                result: Err(DriverError {
                    kind: ErrorKind::Failed,
                    ..
                })
            })
        ));
        assert!(matches!(
            &lines[1],
            DriverLine::Response(ResponseLine {
                id: 7,
                result: Ok(Reply::Apps { .. })
            })
        ));
    }

    #[test]
    fn shutdown_gets_done_and_stops_before_the_next_request() {
        let capture = Capture::default();
        let mut handler = FakeDesktop::with_textedit();
        handler.turn_active = true;
        serve(Cursor::new("{\"id\":1,\"method\":\"shutdown\"}\n{\"id\":2,\"method\":\"turn\",\"active\":true}\n"), LineWriter::new(capture.clone()), &mut handler).unwrap();
        let bytes = capture.0.lock().unwrap().clone();
        assert_eq!(String::from_utf8(bytes).unwrap().lines().count(), 1);
        assert!(!handler.turn_active);
    }

    #[test]
    fn shutdown_runs_when_reading_or_writing_fails() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("broken output"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        impl io::Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("broken input"))
            }
        }
        let mut handler = FakeDesktop::with_textedit();
        handler.turn_active = true;
        assert!(
            serve(
                Cursor::new("{\"id\":1,\"method\":\"list_apps\"}\n"),
                LineWriter::new(Broken),
                &mut handler
            )
            .is_err()
        );
        assert!(!handler.turn_active);
        handler.turn_active = true;
        assert!(
            serve(
                io::BufReader::new(Broken),
                LineWriter::new(Capture::default()),
                &mut handler
            )
            .is_err()
        );
        assert!(!handler.turn_active);
    }

    #[test]
    fn cloned_writers_keep_concurrent_lines_whole_and_flush() {
        #[derive(Clone, Default)]
        struct Flushed {
            capture: Capture,
            flushes: Arc<Mutex<usize>>,
        }
        impl Write for Flushed {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.capture.write(bytes)
            }
            fn flush(&mut self) -> io::Result<()> {
                *self.flushes.lock().unwrap() += 1;
                Ok(())
            }
        }
        let capture = Flushed::default();
        let writer = LineWriter::new(capture.clone());
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let writer = writer.clone();
                scope.spawn(move || {
                    for _ in 0..20 {
                        writer
                            .send(&DriverLine::Event(EventLine {
                                event: DriverEvent::Esc,
                            }))
                            .unwrap();
                    }
                });
            }
        });
        let bytes = capture.capture.0.lock().unwrap().clone();
        assert_eq!(bytes.last(), Some(&b'\n'));
        let text = String::from_utf8(bytes).unwrap();
        assert_eq!(text.lines().count(), 80);
        for line in text.lines() {
            assert!(matches!(
                serde_json::from_str::<DriverLine>(line).unwrap(),
                DriverLine::Event(_)
            ));
        }
        assert_eq!(*capture.flushes.lock().unwrap(), 80);
    }
}
