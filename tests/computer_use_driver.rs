#![cfg(target_os = "macos")]

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::thread;
use std::time::{Duration, Instant};

struct Driver {
    child: Child,
    lines: Receiver<serde_json::Value>,
}

impl Driver {
    fn start(home: &std::path::Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_orca"))
            .arg("computer-use-driver")
            .env("ORCA_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (send, lines) = channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let line = serde_json::from_str(&line.unwrap()).unwrap();
                if send.send(line).is_err() {
                    break;
                }
            }
        });
        Self { child, lines }
    }

    fn call(&mut self, request: serde_json::Value) -> serde_json::Value {
        let input = self.child.stdin.as_mut().unwrap();
        writeln!(input, "{request}").unwrap();
        input.flush().unwrap();
        let response = self.lines.recv_timeout(Duration::from_secs(30)).unwrap();
        assert_eq!(response["id"], request["id"]);
        assert!(response["result"]["Err"].is_null(), "{response}");
        response["result"]["Ok"].clone()
    }

    fn close_stdin_and_wait(&mut self) {
        drop(self.child.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert_eq!(status.code(), Some(0));
                return;
            }
            assert!(
                Instant::now() < deadline,
                "driver did not exit within two seconds of EOF"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        drop(self.child.stdin.take());
        if self.child.try_wait().unwrap().is_none() {
            self.child.kill().unwrap();
        }
        self.child.wait().unwrap();
    }
}

#[test]
#[ignore]
fn closing_stdin_mid_session_releases_everything() {
    let directory = tempfile::tempdir().unwrap();
    let mut driver = Driver::start(directory.path());
    let hello = driver.call(serde_json::json!({"id":1,"method":"hello","protocol":1}));
    if hello["permissions"]["event_tap"] != true {
        eprintln!("skipped: missing Input Monitoring permission");
        return;
    }
    assert_eq!(
        driver.call(serde_json::json!({"id":2,"method":"turn","active":true}))["kind"],
        "done"
    );
    driver.close_stdin_and_wait();
    let mut next = Driver::start(directory.path());
    assert_eq!(
        next.call(serde_json::json!({"id":1,"method":"hello","protocol":1}))["kind"],
        "hello"
    );
    // A second installed tap exercises teardown as well as lock release.
    assert_eq!(
        next.call(serde_json::json!({"id":2,"method":"turn","active":true}))["kind"],
        "done"
    );
    next.close_stdin_and_wait();
}
