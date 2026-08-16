use std::io;

pub(crate) fn finish_tui_run<R>(
    renderer_result: io::Result<R>,
    shutdown_renderer: impl FnOnce(),
    shutdown_inbox: impl FnOnce(),
    shutdown_agent: impl FnOnce() -> io::Result<()>,
) -> io::Result<R> {
    shutdown_renderer();
    shutdown_inbox();
    let agent_result = shutdown_agent();
    match renderer_result {
        Err(error) => Err(error),
        Ok(value) => agent_result.map(|()| value),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::io;
    use std::rc::Rc;

    use super::finish_tui_run;

    #[test]
    fn renderer_error_still_runs_all_shutdown_and_remains_primary() {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let renderer_calls = Rc::clone(&calls);
        let inbox_calls = Rc::clone(&calls);
        let agent_calls = Rc::clone(&calls);

        let error = finish_tui_run::<i32>(
            Err(io::Error::other("renderer failed")),
            move || renderer_calls.borrow_mut().push("renderer"),
            move || inbox_calls.borrow_mut().push("inbox"),
            move || {
                agent_calls.borrow_mut().push("agent");
                Err(io::Error::other("agent failed"))
            },
        )
        .expect_err("renderer error should remain primary after shutdown");

        assert_eq!(error.to_string(), "renderer failed");
        assert_eq!(*calls.borrow(), ["renderer", "inbox", "agent"]);
    }

    #[test]
    fn successful_renderer_returns_agent_shutdown_error_after_all_cleanup() {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let renderer_calls = Rc::clone(&calls);
        let inbox_calls = Rc::clone(&calls);
        let agent_calls = Rc::clone(&calls);

        let error = finish_tui_run(
            Ok(42),
            move || renderer_calls.borrow_mut().push("renderer"),
            move || inbox_calls.borrow_mut().push("inbox"),
            move || {
                agent_calls.borrow_mut().push("agent");
                Err(io::Error::other("agent failed"))
            },
        )
        .expect_err("agent shutdown error should follow successful rendering");

        assert_eq!(error.to_string(), "agent failed");
        assert_eq!(*calls.borrow(), ["renderer", "inbox", "agent"]);
    }
}
