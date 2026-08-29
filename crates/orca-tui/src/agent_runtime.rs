use std::io;
use std::thread::{self, JoinHandle};

use crossbeam_channel::{Receiver, Sender};
use orca_runtime::runtime_host::{RuntimeHost, RuntimeHostHandle};

use crate::action_dispatcher::{InteractionResponseAck, TuiActionDispatcher};
use crate::channels::USER_ACTION_CAPACITY;
use crate::operation_controller::TuiSurfaceTaskControl;
use crate::protocol::{TuiEvent, UserAction};

pub(crate) struct TuiAgentRuntime {
    controller: TuiSurfaceTaskControl,
    dispatcher: TuiActionDispatcher,
    agent: Option<JoinHandle<()>>,
    host: Option<RuntimeHost>,
}

impl TuiAgentRuntime {
    pub(crate) fn spawn_hosted(
        action_rx: Receiver<UserAction>,
        event_tx: Sender<TuiEvent>,
        task_capacity: usize,
        control: TuiSurfaceTaskControl,
        run: impl FnOnce(TuiSurfaceTaskControl, Receiver<UserAction>, RuntimeHostHandle)
        + Send
        + 'static,
    ) -> io::Result<Self> {
        let host = RuntimeHost::start_with_background_capacity(task_capacity)
            .map_err(runtime_host_error)?;
        Self::spawn_with_dispatch_capacities(
            action_rx,
            event_tx,
            USER_ACTION_CAPACITY,
            USER_ACTION_CAPACITY,
            control,
            host,
            run,
        )
    }

    fn spawn_with_dispatch_capacities(
        action_rx: Receiver<UserAction>,
        event_tx: Sender<TuiEvent>,
        command_capacity: usize,
        backlog_capacity: usize,
        control: TuiSurfaceTaskControl,
        host: RuntimeHost,
        run: impl FnOnce(TuiSurfaceTaskControl, Receiver<UserAction>, RuntimeHostHandle)
        + Send
        + 'static,
    ) -> io::Result<Self> {
        let host_handle = host.handle();
        let (mut dispatcher, command_rx) = TuiActionDispatcher::spawn(
            action_rx,
            event_tx,
            control.clone(),
            command_capacity,
            backlog_capacity,
        )?;
        let agent_control = control.clone();
        let agent = thread::Builder::new()
            .name("orca-tui-agent".to_string())
            .spawn(move || run(agent_control, command_rx, host_handle));
        let agent = match agent {
            Ok(agent) => agent,
            Err(error) => {
                let _ = dispatcher.shutdown();
                return Err(error);
            }
        };
        Ok(Self {
            controller: control,
            dispatcher,
            agent: Some(agent),
            host: Some(host),
        })
    }

    #[cfg(test)]
    pub(crate) fn controller(&self) -> &TuiSurfaceTaskControl {
        &self.controller
    }

    pub(crate) fn interaction_ack_receiver(&self) -> Receiver<InteractionResponseAck> {
        self.dispatcher.interaction_ack_receiver()
    }

    pub(crate) fn shutdown(&mut self) -> io::Result<()> {
        let Some(agent) = self.agent.take() else {
            let dispatcher_result = self.dispatcher.shutdown();
            let host_result = self
                .host
                .take()
                .map_or(Ok(()), RuntimeHost::shutdown)
                .map_err(runtime_host_error);
            return dispatcher_result.and(host_result);
        };
        self.controller.shutdown();
        let dispatcher_result = self.dispatcher.shutdown();

        let agent_result = agent
            .join()
            .map_err(|_| io::Error::other("TUI agent controller panicked during shutdown"));
        let host_result = self
            .host
            .take()
            .map_or(Ok(()), RuntimeHost::shutdown)
            .map_err(runtime_host_error);
        dispatcher_result.and(agent_result).and(host_result)
    }
}

fn runtime_host_error(error: orca_runtime::runtime_host::RuntimeHostError) -> io::Error {
    io::Error::other(error.to_string())
}

impl Drop for TuiAgentRuntime {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use orca_runtime::runtime_host::HostedTurnRequest;

    use super::*;
    use crate::surface_actions::TuiSurfaceActions;

    const TEST_ACTIVATION_OBSERVER_TIMEOUT: Duration = Duration::from_secs(10);
    const TEST_OPERATION_START_TIMEOUT: Duration = Duration::from_secs(15);
    const TEST_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

    fn run_blocking_surface_operation(
        control: TuiSurfaceTaskControl,
        host: RuntimeHostHandle,
        event_tx: Sender<TuiEvent>,
        ready_tx: crossbeam_channel::Sender<Result<(), &'static str>>,
    ) {
        let mut config = crate::test_support::test_run_config();
        config.history_mode = orca_core::config::HistoryMode::Record;
        let thread = host
            .start_thread(config.clone(), "agent runtime test")
            .expect("hosted test thread");
        let activation_control = control.clone();
        let activation_ready = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + TEST_ACTIVATION_OBSERVER_TIMEOUT;
            while !activation_control.has_surface_active() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            let readiness = activation_control
                .has_surface_active()
                .then_some(())
                .ok_or("surface operation did not become active before deadline");
            let _ = ready_tx.send(readiness);
        });
        let _ = TuiSurfaceActions::new(thread.typed_surface()).run_turn(
            HostedTurnRequest::new("mock_stream_delay_ms 5000"),
            config,
            &control,
            &event_tx,
        );
        activation_ready.join().expect("activation observer");
    }

    fn spawn_blocking_runtime(
        action_rx: Receiver<UserAction>,
        event_tx: Sender<TuiEvent>,
        ready_tx: crossbeam_channel::Sender<Result<(), &'static str>>,
    ) -> TuiAgentRuntime {
        let controller = TuiSurfaceTaskControl::new();
        let operation_events = event_tx.clone();
        TuiAgentRuntime::spawn_hosted(
            action_rx,
            event_tx,
            1,
            controller,
            move |control, _commands, host| {
                run_blocking_surface_operation(control, host, operation_events, ready_tx)
            },
        )
        .expect("hosted agent runtime spawned")
    }

    #[test]
    fn shutdown_cancels_current_operation_and_joins_agent_thread() {
        let _home = crate::test_support::isolate_orca_home();
        let (_action_tx, action_rx) = crossbeam_channel::bounded(1);
        let (event_tx, _event_rx) = crossbeam_channel::unbounded();
        let (ready_tx, ready_rx) = crossbeam_channel::bounded(1);

        let mut runtime = spawn_blocking_runtime(action_rx, event_tx, ready_tx);

        ready_rx
            .recv_timeout(TEST_OPERATION_START_TIMEOUT)
            .expect("activation observer completed")
            .expect("agent surface operation became active");
        runtime.shutdown().expect("agent runtime shutdown");
    }

    #[test]
    fn drop_uses_the_same_cancel_and_join_path() {
        let _home = crate::test_support::isolate_orca_home();
        let (_action_tx, action_rx) = crossbeam_channel::bounded(1);
        let (event_tx, _event_rx) = crossbeam_channel::unbounded();
        let (ready_tx, ready_rx) = crossbeam_channel::bounded(1);

        let runtime = spawn_blocking_runtime(action_rx, event_tx, ready_tx);

        ready_rx
            .recv_timeout(TEST_OPERATION_START_TIMEOUT)
            .expect("activation observer completed")
            .expect("agent surface operation became active");
        drop(runtime);
    }

    #[test]
    fn shutdown_does_not_wait_for_capacity_in_full_action_mailbox() {
        let _home = crate::test_support::isolate_orca_home();
        let (action_tx, action_rx) = crossbeam_channel::bounded(1);
        let (event_tx, _event_rx) = crossbeam_channel::unbounded();
        let operation_events = event_tx.clone();
        action_tx
            .send(UserAction::Submit("fill command mailbox".to_string()))
            .expect("fill action mailbox");
        let (ready_tx, ready_rx) = crossbeam_channel::bounded(1);
        let control = TuiSurfaceTaskControl::isolated_for_test();
        let host = RuntimeHost::start().expect("runtime host");

        let mut runtime = TuiAgentRuntime::spawn_with_dispatch_capacities(
            action_rx,
            event_tx,
            1,
            1,
            control,
            host,
            move |control, _commands, host| {
                run_blocking_surface_operation(control, host, operation_events, ready_tx)
            },
        )
        .expect("agent runtime spawned");
        ready_rx
            .recv_timeout(TEST_OPERATION_START_TIMEOUT)
            .expect("activation observer completed")
            .expect("agent surface operation became active");

        let (done_tx, done_rx) = crossbeam_channel::bounded(1);
        let shutdown = std::thread::spawn(move || {
            let result = runtime.shutdown();
            done_tx.send(result).expect("shutdown result");
        });
        let result = done_rx.recv_timeout(TEST_SHUTDOWN_TIMEOUT);

        shutdown.join().expect("shutdown thread joined");
        result
            .expect("shutdown must not wait for action mailbox capacity")
            .expect("runtime shutdown");
    }
}
