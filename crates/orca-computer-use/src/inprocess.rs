//! Native pipes keep the live-test driver's permissions in the host terminal process tree.
use std::io::{self, BufReader};
use std::sync::Mutex;
use std::thread;

use crate::client::{DriverLauncher, LaunchedDriver};
use crate::serve::{DriverHandler, LineWriter, serve};

pub struct InProcessLauncher<H> {
    handler: Mutex<Option<H>>,
}

impl<H> InProcessLauncher<H> {
    pub fn new(handler: H) -> Self {
        Self {
            handler: Mutex::new(Some(handler)),
        }
    }
}

impl<H: DriverHandler + Send + 'static> DriverLauncher for InProcessLauncher<H> {
    fn launch(&self) -> io::Result<LaunchedDriver> {
        let (input, stdin) = io::pipe()?;
        let (stdout, output) = io::pipe()?;
        let mut handler = self
            .handler
            .lock()
            .map_err(|_| io::Error::other("in-process driver lock poisoned"))?
            .take()
            .ok_or_else(|| io::Error::other("in-process driver already launched"))?;
        thread::Builder::new()
            .name("computer-use-driver".into())
            .spawn(move || {
                let _ = serve(BufReader::new(input), LineWriter::new(output), &mut handler);
            })?;
        Ok(LaunchedDriver {
            stdin: Box::new(stdin),
            stdout: Box::new(stdout),
            child: None,
        })
    }
}
