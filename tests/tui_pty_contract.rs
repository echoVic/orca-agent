#![cfg(unix)]

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use base64::Engine as _;

const PROMPT: &str = "typed TUI PTY submit";
const ASSISTANT_SENTINEL: &str = "Mock runtime completed the headless harness contract.";

#[test]
fn tui_submit_renders_and_restores_the_terminal() {
    let home = tempfile::tempdir().expect("temporary ORCA_HOME");
    let cwd = tempfile::tempdir().expect("temporary workspace");
    let mut process = PtyProcess::spawn(home.path(), cwd.path()).expect("spawn TUI in PTY");

    let mut output = Vec::new();
    accept_new_workspace(&mut process, &mut output);
    receive_until(
        &process,
        &mut output,
        ASSISTANT_SENTINEL,
        Duration::from_secs(10),
        "TUI did not render the typed assistant terminal",
    );

    arm_idle_exit(&mut process, &mut output);

    let status = process.wait_for_exit(Duration::from_secs(5));
    process.close_io_and_join();
    process.drain_output(&mut output);

    assert_eq!(status.code(), Some(130), "TUI exited with {status}");
    assert!(
        output
            .windows(b"\x1b[?1049h".len())
            .any(|window| window == b"\x1b[?1049h"),
        "TUI did not enter the alternate screen"
    );
    assert!(
        output
            .windows(b"\x1b[?1049l".len())
            .any(|window| window == b"\x1b[?1049l"),
        "TUI did not restore the primary screen"
    );
}

#[test]
fn tui_permission_round_trips_through_the_runtime_surface() {
    if !matches!(
        orca_tools::sandbox::enforcement_state(),
        orca_core::capability::EnforcementState::Enforced
    ) {
        // This contract verifies a successful approval followed by a real
        // sandboxed process. Hosts that cannot install their OS backend must
        // reject execution, so the success-path integration is inapplicable.
        return;
    }
    let home = tempfile::tempdir().expect("temporary ORCA_HOME");
    let cwd = tempfile::tempdir().expect("temporary workspace");
    std::fs::write(home.path().join("config.toml"), "mode = \"suggest\"\n")
        .expect("configure explicit suggest mode");
    const PERMISSION_SENTINEL: &str = "PTY_PERMISSION_RESUMED";
    let prompt = format!(
        "request_permissions_then_bash {} :: printf '\\120\\124\\131\\137\\120\\105\\122\\115\\111\\123\\123\\111\\117\\116\\137\\122\\105\\123\\125\\115\\105\\104'",
        cwd.path().display()
    );
    assert!(
        !prompt.contains(PERMISSION_SENTINEL),
        "the post-permission sentinel must not be present in the rendered prompt"
    );
    let mut process = PtyProcess::spawn_with_prompt(home.path(), cwd.path(), &prompt)
        .expect("spawn permission TUI in PTY");

    let mut output = Vec::new();
    accept_new_workspace(&mut process, &mut output);
    receive_until(
        &process,
        &mut output,
        "Capability Boundary",
        Duration::from_secs(10),
        "TUI did not render the runtime-owned permission",
    );
    process.write(b"1").expect("allow permission once");
    receive_until(
        &process,
        &mut output,
        "requested shell",
        Duration::from_secs(10),
        "TUI did not advance to the runtime-owned tool approval",
    );
    process.write(b"1").expect("approve bash once");
    assert_screen_shows_wrapped_token(
        &process,
        &mut output,
        PERMISSION_SENTINEL,
        "TUI did not resume after the typed capability response",
    );
    assert_screen_shows(
        &process,
        &mut output,
        "Mock completed after tool execution.",
        "TUI did not complete after the approved tool execution",
    );

    arm_idle_exit(&mut process, &mut output);
    let status = process.wait_for_exit(Duration::from_secs(5));
    process.close_io_and_join();
    assert_eq!(status.code(), Some(130), "TUI exited with {status}");
}

#[test]
fn tui_cancel_returns_to_idle_through_the_runtime_surface() {
    let home = tempfile::tempdir().expect("temporary ORCA_HOME");
    let cwd = tempfile::tempdir().expect("temporary workspace");
    let mut process =
        PtyProcess::spawn_with_prompt(home.path(), cwd.path(), "mock_stream_delay_ms 10000")
            .expect("spawn cancellable TUI in PTY");

    let mut output = Vec::new();
    accept_new_workspace(&mut process, &mut output);
    receive_until(
        &process,
        &mut output,
        "Running 0s",
        Duration::from_secs(20),
        "TUI did not render the active turn before cancellation",
    );
    cancel_running_turn_and_exit(&mut process, &mut output);

    let status = process.wait_for_exit(Duration::from_secs(5));
    process.close_io_and_join();
    process.drain_output(&mut output);
    assert_eq!(status.code(), Some(130), "TUI exited with {status}");
    assert!(
        !String::from_utf8_lossy(&output).contains("Mock slow stream completed."),
        "cancelled PTY turn must not display a post-terminal completion; output={}",
        String::from_utf8_lossy(&output)
    );
}

#[test]
fn tui_tasks_workspace_stops_one_detached_subagent_without_terminal_spam() {
    let home = tempfile::tempdir().expect("temporary ORCA_HOME");
    let cwd = tempfile::tempdir().expect("temporary workspace");
    std::fs::write(home.path().join("config.toml"), "mode = \"full-auto\"\n")
        .expect("configure full-auto mode");
    let release_marker = cwd.path().join("release-detached-subagent");
    let mut process = PtyProcess::spawn_with_prompt(
        home.path(),
        cwd.path(),
        &format!(
            "subagent mock_stream_release_marker {}",
            release_marker.display()
        ),
    )
    .expect("spawn detached-subagent TUI in PTY");
    let mut output = Vec::new();
    accept_new_workspace(&mut process, &mut output);
    assert_screen_shows(
        &process,
        &mut output,
        "Agents 1 active",
        "parent did not expose the running detached child",
    );

    process.write(b"/agents\r").expect("open Tasks workspace");
    assert_screen_shows(
        &process,
        &mut output,
        "Tasks Workspace",
        "TUI did not open the unified Tasks workspace",
    );
    assert_screen_shows(
        &process,
        &mut output,
        "mock_stream_release_marker",
        "Tasks workspace did not expose the detached child",
    );
    process.write(b"s").expect("stop selected subagent");
    assert_screen_shows(
        &process,
        &mut output,
        "stopped by user",
        "Tasks workspace did not converge to the actor-owned stop activity",
    );

    process.write(&[0x1b]).expect("close Tasks workspace");
    std::thread::sleep(Duration::from_millis(400));
    process.drain_output(&mut output);
    assert!(
        process
            .try_wait()
            .expect("poll TUI after task stop")
            .is_none(),
        "stopping a child must keep the parent TUI alive"
    );
    let rendered = String::from_utf8_lossy(&output);
    assert!(
        rendered.matches("Stopping ").count() <= 3,
        "subagent stop notice was rendered in a loop; output={rendered}"
    );

    arm_idle_exit(&mut process, &mut output);
    let status = process.wait_for_exit(Duration::from_secs(5));
    process.close_io_and_join();
    assert_eq!(status.code(), Some(130), "TUI exited with {status}");
}

#[test]
fn tui_docks_an_agent_that_a_queued_message_starts() {
    let home = tempfile::tempdir().expect("temporary ORCA_HOME");
    let cwd = tempfile::tempdir().expect("temporary workspace");
    std::fs::write(home.path().join("config.toml"), "mode = \"full-auto\"\n")
        .expect("configure full-auto mode");
    let release_marker = cwd.path().join("release-queued-subagent");
    let mut process =
        PtyProcess::spawn_with_prompt(home.path(), cwd.path(), "mock_stream_delay_ms 1500")
            .expect("spawn TUI in PTY");
    let mut output = Vec::new();
    accept_new_workspace(&mut process, &mut output);
    assert_screen_shows(
        &process,
        &mut output,
        "Mock slow stream started.",
        "the first turn did not start",
    );
    process
        .write(
            format!(
                "subagent mock_stream_release_marker {}\r",
                release_marker.display()
            )
            .as_bytes(),
        )
        .expect("queue a message that starts an agent");
    assert_screen_shows(
        &process,
        &mut output,
        "Agents 1 active",
        "the agent a queued message started never reached the dock",
    );

    std::fs::write(&release_marker, b"release").expect("release the agent");
    arm_idle_exit(&mut process, &mut output);
    let status = process.wait_for_exit(Duration::from_secs(10));
    process.close_io_and_join();
    assert_eq!(status.code(), Some(130), "TUI exited with {status}");
}

#[test]
fn tui_shows_a_background_agents_progress_while_the_parent_turn_waits_on_it() {
    let home = tempfile::tempdir().expect("temporary ORCA_HOME");
    let cwd = tempfile::tempdir().expect("temporary workspace");
    std::fs::write(home.path().join("config.toml"), "mode = \"full-auto\"\n")
        .expect("configure full-auto mode");
    let release_marker = cwd.path().join("release-background-agent");
    let mut process = PtyProcess::spawn_with_prompt(
        home.path(),
        cwd.path(),
        &format!(
            "subagent async mock_stream_release_marker {}",
            release_marker.display()
        ),
    )
    .expect("spawn background-agent TUI in PTY");
    let mut output = Vec::new();
    accept_new_workspace(&mut process, &mut output);
    assert_screen_shows(
        &process,
        &mut output,
        "Agents 1 active",
        "parent did not expose the running background agent",
    );
    // The agent is parked on its marker and the parent turn waits for it, so
    // its progress can only arrive through the relay poll of a running turn,
    // which used to be starved until the turn ended.
    assert_screen_shows(
        &process,
        &mut output,
        "thinking,",
        "the background agent's progress did not reach the screen while the parent turn ran",
    );

    std::fs::write(&release_marker, b"release").expect("release background agent");
    assert_screen_shows(
        &process,
        &mut output,
        "Agent completed",
        "the background agent's completion did not reach the screen",
    );

    arm_idle_exit(&mut process, &mut output);
    let status = process.wait_for_exit(Duration::from_secs(5));
    process.close_io_and_join();
    assert_eq!(status.code(), Some(130), "TUI exited with {status}");
}

#[test]
fn tui_click_on_a_running_background_agent_opens_its_transcript() {
    let home = tempfile::tempdir().expect("temporary ORCA_HOME");
    let cwd = tempfile::tempdir().expect("temporary workspace");
    std::fs::write(home.path().join("config.toml"), "mode = \"full-auto\"\n")
        .expect("configure full-auto mode");
    let release_marker = cwd.path().join("release-clicked-agent");
    let mut process = PtyProcess::spawn_with_prompt(
        home.path(),
        cwd.path(),
        &format!(
            "subagent async mock_stream_release_marker {}",
            release_marker.display()
        ),
    )
    .expect("spawn background-agent TUI in PTY");
    let mut output = Vec::new();
    accept_new_workspace(&mut process, &mut output);
    assert_screen_shows(
        &process,
        &mut output,
        "Agents 1 active",
        "parent did not expose the running background agent",
    );

    // The agent's row in the dock under the transcript; the transcript's
    // own echo of the prompt carries the "subagent" prefix.
    let screen = reconstruct_screen(&output);
    let row = screen
        .lines()
        .position(|line| line.contains("mock_stream_release_marker") && !line.contains("subagent"))
        .map(|index| index + 1)
        .unwrap_or_else(|| panic!("the dock does not list the agent; screen=\n{screen}"));
    process
        .write(format!("\x1b[<0;6;{row}M\x1b[<0;6;{row}m").as_bytes())
        .expect("click the running agent");
    assert_screen_shows(
        &process,
        &mut output,
        "Agent Transcript",
        "clicking the running agent did not open its transcript",
    );
    // The parent turn keeps waiting on the agent, and the panel is answered
    // anyway: the agent is still in its first step, so its live activity.
    assert_screen_shows(
        &process,
        &mut output,
        "No transcript checkpoint yet",
        "the running agent's panel was not answered while the parent turn ran",
    );

    std::fs::write(&release_marker, b"release").expect("release background agent");
    arm_idle_exit(&mut process, &mut output);
    let status = process.wait_for_exit(Duration::from_secs(5));
    process.close_io_and_join();
    assert_eq!(status.code(), Some(130), "TUI exited with {status}");
}

#[test]
fn tui_escape_cancels_a_running_subagent_and_keeps_the_parent_usable() {
    let home = tempfile::tempdir().expect("temporary ORCA_HOME");
    let cwd = tempfile::tempdir().expect("temporary workspace");
    std::fs::write(home.path().join("config.toml"), "mode = \"full-auto\"\n")
        .expect("configure full-auto mode");
    let mut process = PtyProcess::spawn_with_prompt(
        home.path(),
        cwd.path(),
        "subagent mock_stream_delay_ms 30000",
    )
    .expect("spawn foreground-subagent TUI in PTY");
    let mut output = Vec::new();
    accept_new_workspace(&mut process, &mut output);
    assert_screen_shows(
        &process,
        &mut output,
        "Agents 1 active",
        "TUI did not expose the running foreground child",
    );

    let cancel_start = output.len();
    process
        .write(&[0x1b])
        .expect("cancel running turn with Esc");
    std::thread::sleep(Duration::from_millis(750));
    process.drain_output(&mut output);
    assert!(
        process.try_wait().expect("poll TUI after Esc").is_none(),
        "Esc cancellation must not terminate the parent TUI"
    );
    assert!(
        output.len().saturating_sub(cancel_start) < 512 * 1024,
        "Esc cancellation produced an unbounded redraw loop"
    );
    assert!(
        !String::from_utf8_lossy(&output[cancel_start..]).contains("Mock slow stream completed."),
        "cancelled subagent emitted a post-terminal completion"
    );
    // Under load the cancel can take longer than the pause above; a follow-up
    // typed before it lands is queued, and an interrupt pauses the queue by
    // design. Type the follow-up once the parent is visibly idle again.
    let deadline = Instant::now() + Duration::from_secs(10);
    while screen_contains(&output, "Esc interrupt") {
        assert!(
            Instant::now() < deadline,
            "Esc did not return the parent to idle; reconstructed screen=\n{}",
            reconstruct_screen(&output)
        );
        if let Some(chunk) = process.receive_output(Duration::from_millis(250)) {
            output.extend_from_slice(&chunk);
        }
    }

    let follow_up_start = output.len();
    process
        .write(b"parent remains usable\r")
        .expect("submit follow-up after Esc");
    receive_until_after(
        &process,
        &mut output,
        ASSISTANT_SENTINEL,
        follow_up_start,
        Duration::from_secs(10),
        "parent TUI did not accept a new turn after subagent cancellation",
    );

    arm_idle_exit(&mut process, &mut output);
    let status = process.wait_for_exit(Duration::from_secs(5));
    process.close_io_and_join();
    assert_eq!(status.code(), Some(130), "TUI exited with {status}");
}

#[test]
fn tui_bracketed_image_path_paste_materializes_an_atomic_attachment() {
    let home = tempfile::tempdir().expect("temporary ORCA_HOME");
    let cwd = tempfile::tempdir().expect("temporary workspace");
    let image_path = cwd.path().join("pty pasted image.png");
    let png = base64::engine::general_purpose::STANDARD
        .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=")
        .expect("decode PNG fixture");
    std::fs::write(&image_path, png).expect("write PNG fixture");
    let mut process = PtyProcess::spawn_with_model_and_prompt(
        home.path(),
        cwd.path(),
        orca_core::model::VISION_MODEL,
        "mock_stream_delay_ms 10000",
    )
    .expect("spawn vision TUI in PTY");
    let mut output = Vec::new();
    accept_new_workspace(&mut process, &mut output);
    receive_until(
        &process,
        &mut output,
        "Running 0s",
        Duration::from_secs(20),
        "TUI did not render the active turn before image paste",
    );

    process
        .write(format!("\x1b[200~{}\x1b[201~", image_path.display()).as_bytes())
        .expect("paste image path");
    receive_until(
        &process,
        &mut output,
        "[Image #1]",
        Duration::from_secs(10),
        "TUI did not materialize the pasted image attachment",
    );

    process.write(&[0x15]).expect("clear composer with Ctrl+U");
    cancel_running_turn_and_exit(&mut process, &mut output);
    let status = process.wait_for_exit(Duration::from_secs(5));
    process.close_io_and_join();
    assert_eq!(status.code(), Some(130), "TUI exited with {status}");
}

#[test]
fn tui_restart_recovers_history_from_the_runtime_snapshot() {
    let home = tempfile::tempdir().expect("temporary ORCA_HOME");
    let cwd = tempfile::tempdir().expect("temporary workspace");
    let mut source = PtyProcess::spawn_with_prompt(home.path(), cwd.path(), "pty restart seed")
        .expect("spawn source TUI in PTY");
    let mut source_output = Vec::new();
    accept_new_workspace(&mut source, &mut source_output);
    receive_until(
        &source,
        &mut source_output,
        ASSISTANT_SENTINEL,
        Duration::from_secs(10),
        "source TUI did not complete",
    );
    arm_idle_exit(&mut source, &mut source_output);
    let status = source.wait_for_exit(Duration::from_secs(5));
    source.close_io_and_join();
    assert_eq!(status.code(), Some(130), "source TUI exited with {status}");

    let mut resumed =
        PtyProcess::spawn_resumed(home.path(), cwd.path(), "latest", "mock_history_echo")
            .expect("spawn resumed TUI in PTY");
    let mut resumed_output = Vec::new();
    receive_until(
        &resumed,
        &mut resumed_output,
        "Mock history users: pty restart seed | mock_history_echo",
        Duration::from_secs(10),
        "resumed TUI did not hydrate history from the typed snapshot",
    );
    arm_idle_exit(&mut resumed, &mut resumed_output);
    let status = resumed.wait_for_exit(Duration::from_secs(5));
    resumed.close_io_and_join();
    assert_eq!(status.code(), Some(130), "resumed TUI exited with {status}");
}

#[test]
fn tui_side_conversation_is_separate_disposable_and_returns_to_parent() {
    let home = tempfile::tempdir().expect("temporary ORCA_HOME");
    let cwd = tempfile::tempdir().expect("temporary workspace");
    let mut process = PtyProcess::spawn_with_prompt(home.path(), cwd.path(), "main pty seed")
        .expect("spawn parent TUI in PTY");
    let mut output = Vec::new();
    accept_new_workspace(&mut process, &mut output);
    receive_until(
        &process,
        &mut output,
        ASSISTANT_SENTINEL,
        Duration::from_secs(10),
        "parent TUI did not complete before Side opened",
    );

    // The slash command opens the empty Side composer. The shortcut resolver is
    // covered separately; this synthetic PTY does not emulate Kitty negotiation.
    process
        .write(b"/side\r")
        .expect("open Side with slash command");
    receive_until(
        &process,
        &mut output,
        "Ctrl+/ back",
        Duration::from_secs(5),
        "TUI did not open Side",
    );
    process
        .write(b"mock_history_echo\r")
        .expect("submit Side question");
    receive_until(
        &process,
        &mut output,
        "Mock history users: main pty seed | mock_history_echo",
        Duration::from_secs(10),
        "Side did not inherit the parent cutover context",
    );

    // Toggling back must restore the parent projection. A parent history echo
    // must not contain the Side-only prompt.
    process
        .write(b"\x1b[47;5u")
        .expect("return to parent with Ctrl+/");
    let parent_toggle_start = output.len();
    receive_until_after(
        &process,
        &mut output,
        "Side · Ctrl+/",
        parent_toggle_start,
        Duration::from_secs(5),
        "TUI did not restore the parent while retaining Side",
    );
    let parent_echo_start = output.len();
    process
        .write(b"mock_history_echo\r")
        .expect("submit parent history check");
    receive_until_after(
        &process,
        &mut output,
        "Mock history users: main pty seed | mock_history_echo",
        parent_echo_start,
        Duration::from_secs(10),
        "parent did not resume after Side toggle",
    );
    assert!(
        !String::from_utf8_lossy(&output[parent_echo_start..])
            .contains("main pty seed | mock_history_echo | mock_history_echo"),
        "Side prompt leaked into the parent transcript"
    );

    // Return to Side and close it. Ctrl+C owns Side cleanup and must not
    // interrupt or close the parent.
    let side_toggle_start = output.len();
    process.write(b"\x1b[47;5u").expect("return to Side");
    receive_until_after(
        &process,
        &mut output,
        "Ctrl+/ back",
        side_toggle_start,
        Duration::from_secs(5),
        "TUI did not reactivate Side",
    );
    process.write(&[0x03]).expect("close Side with Ctrl+C");
    std::thread::sleep(Duration::from_millis(250));
    process.drain_output(&mut output);
    let parent_after_close_start = output.len();
    process
        .write(b"mock_history_echo\r")
        .expect("submit after closing Side");
    receive_until_after(
        &process,
        &mut output,
        "Mock history users: main pty seed | mock_history_echo | mock_history_echo",
        parent_after_close_start,
        Duration::from_secs(10),
        "closing Side did not restore the parent",
    );
    assert!(
        !String::from_utf8_lossy(&output[parent_after_close_start..])
            .contains("main pty seed | mock_history_echo | mock_history_echo | mock_history_echo"),
        "Side prompt leaked into the parent after close"
    );

    arm_idle_exit(&mut process, &mut output);
    let status = process.wait_for_exit(Duration::from_secs(5));
    process.close_io_and_join();
    assert_eq!(status.code(), Some(130), "TUI exited with {status}");

    let session_files = count_history_files(&home.path().join("sessions"));
    assert_eq!(session_files, 1, "Side must not create a durable session");
}

// Regression: switching panes must not blank the transcript. The existing
// coverage always submits a fresh prompt after each toggle, which forces a
// re-render and hides whether the toggle itself left the pane empty. This test
// toggles and then inspects the screen WITHOUT submitting anything.
#[test]
fn tui_side_toggle_keeps_transcripts_visible_without_resubmitting() {
    let home = tempfile::tempdir().expect("temporary ORCA_HOME");
    let cwd = tempfile::tempdir().expect("temporary workspace");
    let mut process = PtyProcess::spawn_with_prompt(home.path(), cwd.path(), "main pty seed")
        .expect("spawn parent TUI in PTY");
    let mut output = Vec::new();
    accept_new_workspace(&mut process, &mut output);
    receive_until(
        &process,
        &mut output,
        ASSISTANT_SENTINEL,
        Duration::from_secs(10),
        "parent TUI did not complete before Side opened",
    );

    // Open Side and echo the inherited history so the Side transcript carries a
    // marker distinct from the parent.
    process
        .write(b"/side\r")
        .expect("open Side with slash command");
    receive_until(
        &process,
        &mut output,
        "Ctrl+/ back",
        Duration::from_secs(5),
        "TUI did not open Side",
    );
    process
        .write(b"mock_history_echo\r")
        .expect("submit Side question");
    receive_until(
        &process,
        &mut output,
        "Mock history users: main pty seed | mock_history_echo",
        Duration::from_secs(10),
        "Side did not inherit the parent cutover context",
    );

    // Toggle back to the parent. Do NOT submit anything. The parent transcript
    // (its seed prompt) must remain on the reconstructed screen after the switch.
    process
        .write(b"\x1b[47;5u")
        .expect("return to parent with Ctrl+/");
    // The mode chip shows only on the parent's status line.
    assert_screen_shows(
        &process,
        &mut output,
        "⇧Tab auto-edit Side · Ctrl+/",
        "TUI did not restore the parent status line",
    );
    assert_screen_shows(
        &process,
        &mut output,
        "›  main pty seed",
        "parent transcript went blank after toggling back without resubmitting",
    );

    // Toggle to Side again. Its inherited echo must remain on the reconstructed
    // screen after the switch, again without submitting.
    process.write(b"\x1b[47;5u").expect("return to Side");
    assert_screen_shows(
        &process,
        &mut output,
        "Ctrl+/ back",
        "TUI did not reactivate the Side status line",
    );
    assert_screen_shows(
        &process,
        &mut output,
        "Mock history users: main pty seed | mock_history_echo",
        "side transcript went blank after toggling back without resubmitting",
    );

    process.write(&[0x03]).expect("close Side with Ctrl+C");
    arm_idle_exit(&mut process, &mut output);
    let status = process.wait_for_exit(Duration::from_secs(5));
    process.close_io_and_join();
    assert_eq!(status.code(), Some(130), "TUI exited with {status}");
}

// Reconstructs the on-screen terminal grid from the full PTY byte stream and
// asserts `expected` is visible. Incremental renderers only emit cells that
// change between frames, so a switch that leaves the top rows untouched will
// not re-print them — checking only the post-switch delta would miss content
// that is genuinely on screen. Rebuilding the grid reflects what the user sees.
#[test]
fn tui_recap_command_draws_the_strip_and_detail_and_esc_closes_the_detail() {
    let home = tempfile::tempdir().expect("temporary ORCA_HOME");
    let cwd = tempfile::tempdir().expect("temporary workspace");
    let mut process = PtyProcess::spawn_with_prompt(home.path(), cwd.path(), "recap pty seed")
        .expect("spawn recap TUI in PTY");
    let mut output = Vec::new();
    accept_new_workspace(&mut process, &mut output);
    receive_until(
        &process,
        &mut output,
        ASSISTANT_SENTINEL,
        Duration::from_secs(10),
        "TUI did not finish the seed turn",
    );

    process.write(b"/recap\r").expect("request a recap");
    // An explicit /recap opens the full text over the transcript, and the
    // strip under it carries the same recap.
    assert_screen_shows(
        &process,
        &mut output,
        "Recap Mock runtime completed the headless harness contract. Esc close",
        "TUI did not open the recap detail",
    );
    assert_screen_shows(
        &process,
        &mut output,
        "↳ Recap",
        "TUI did not draw the recap strip",
    );

    process
        .write(&[0x1b])
        .expect("close the recap detail with Esc");
    let deadline = Instant::now() + Duration::from_secs(5);
    while screen_contains(&output, "Esc close") {
        assert!(
            Instant::now() < deadline,
            "Esc did not close the recap detail; reconstructed screen=\n{}",
            reconstruct_screen(&output)
        );
        if let Some(chunk) = process.receive_output(Duration::from_millis(250)) {
            output.extend_from_slice(&chunk);
        }
    }
    assert!(
        screen_contains(&output, "↳ Recap"),
        "closing the detail must keep the strip; reconstructed screen=\n{}",
        reconstruct_screen(&output)
    );
    assert!(
        process.try_wait().expect("poll TUI after Esc").is_none(),
        "Esc on the recap detail must not end the session"
    );

    arm_idle_exit(&mut process, &mut output);
    let status = process.wait_for_exit(Duration::from_secs(5));
    process.close_io_and_join();
    assert_eq!(status.code(), Some(130), "TUI exited with {status}");
}

#[test]
fn a_prompt_given_on_the_command_line_waits_for_the_workspace_review() {
    let home = tempfile::tempdir().expect("temporary ORCA_HOME");
    let cwd = tempfile::tempdir().expect("temporary workspace");
    let mut process = PtyProcess::spawn(home.path(), cwd.path()).expect("spawn TUI in PTY");

    let mut output = Vec::new();
    receive_until(
        &process,
        &mut output,
        "review this workspace security boundary",
        Duration::from_secs(20),
        "a prompt on the command line skipped the review of a new workspace",
    );
    // A prompt sent at launch would have been answered by now.
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        process.drain_output(&mut output);
        assert!(
            !contains_rendered_text(&output, ASSISTANT_SENTINEL),
            "the prompt ran before the workspace was accepted"
        );
        std::thread::sleep(Duration::from_millis(25));
    }

    process.write(b"\r").expect("trust the workspace");
    receive_until(
        &process,
        &mut output,
        ASSISTANT_SENTINEL,
        Duration::from_secs(10),
        "the prompt did not run once the workspace was accepted",
    );
    arm_idle_exit(&mut process, &mut output);
    let status = process.wait_for_exit(Duration::from_secs(5));
    process.close_io_and_join();
    assert_eq!(status.code(), Some(130), "TUI exited with {status}");
}

#[test]
fn leaving_the_workspace_review_never_runs_the_command_line_prompt() {
    let home = tempfile::tempdir().expect("temporary ORCA_HOME");
    let cwd = tempfile::tempdir().expect("temporary workspace");
    let mut process = PtyProcess::spawn(home.path(), cwd.path()).expect("spawn TUI in PTY");

    let mut output = Vec::new();
    receive_until(
        &process,
        &mut output,
        "review this workspace security boundary",
        Duration::from_secs(20),
        "a prompt on the command line skipped the review of a new workspace",
    );
    process.write(b"e").expect("leave the review");
    let status = process.wait_for_exit(Duration::from_secs(5));
    process.close_io_and_join();
    process.drain_output(&mut output);

    assert_eq!(status.code(), Some(0), "TUI exited with {status}");
    assert!(
        !contains_rendered_text(&output, ASSISTANT_SENTINEL),
        "the prompt ran although the user left the review"
    );
}

#[test]
fn a_conversation_resumed_on_the_command_line_waits_for_the_workspace_review() {
    let home = tempfile::tempdir().expect("temporary ORCA_HOME");
    let _recorded_in = record_a_conversation(home.path(), "pty resume seed");
    let cwd = tempfile::tempdir().expect("temporary workspace");
    let mut process =
        PtyProcess::spawn_resumed(home.path(), cwd.path(), "latest", "mock_history_echo")
            .expect("spawn resumed TUI in PTY");

    let mut output = Vec::new();
    receive_until(
        &process,
        &mut output,
        "review this workspace security boundary",
        Duration::from_secs(20),
        "resuming a conversation skipped the review of a new workspace",
    );
    // A conversation resumed at launch would have loaded by now.
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        process.drain_output(&mut output);
        assert!(
            !contains_rendered_text(&output, "Resumed saved conversation"),
            "the conversation was resumed before the workspace was accepted"
        );
        std::thread::sleep(Duration::from_millis(25));
    }

    process.write(b"\r").expect("trust the workspace");
    receive_until(
        &process,
        &mut output,
        "Mock history users: pty resume seed | mock_history_echo",
        Duration::from_secs(10),
        "the resumed conversation did not get the prompt once the workspace was accepted",
    );
    arm_idle_exit(&mut process, &mut output);
    let status = process.wait_for_exit(Duration::from_secs(5));
    process.close_io_and_join();
    assert_eq!(status.code(), Some(130), "TUI exited with {status}");
}

#[test]
fn leaving_the_workspace_review_never_resumes_the_conversation() {
    let home = tempfile::tempdir().expect("temporary ORCA_HOME");
    let _recorded_in = record_a_conversation(home.path(), "pty resume seed");
    let fixture = tempfile::tempdir().expect("MCP fixture directory");
    let server = SlowMcpServer::configure(home.path(), fixture.path());
    let cwd = tempfile::tempdir().expect("temporary workspace");
    let mut process =
        PtyProcess::spawn_resumed(home.path(), cwd.path(), "latest", "mock_history_echo")
            .expect("spawn resumed TUI in PTY");

    let mut output = Vec::new();
    receive_until(
        &process,
        &mut output,
        "review this workspace security boundary",
        Duration::from_secs(20),
        "resuming a conversation skipped the review of a new workspace",
    );
    // The resumed conversation would have started its MCP server by now.
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        assert_eq!(
            server.started(),
            Vec::<String>::new(),
            "an MCP server started before the workspace was accepted"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    process.write(b"e").expect("leave the review");
    let status = process.wait_for_exit(Duration::from_secs(5));
    process.close_io_and_join();
    process.drain_output(&mut output);

    assert_eq!(status.code(), Some(0), "TUI exited with {status}");
    assert!(
        !contains_rendered_text(&output, "Mock history users"),
        "the prompt ran although the user left the review"
    );
    assert_eq!(
        server.started(),
        Vec::<String>::new(),
        "choosing Exit started an MCP server"
    );
}

#[test]
fn a_conversation_that_cannot_be_resumed_leaves_the_prompt_to_a_new_one() {
    let home = tempfile::tempdir().expect("temporary ORCA_HOME");
    let cwd = tempfile::tempdir().expect("temporary workspace");
    let mut process = PtyProcess::spawn_resumed(
        home.path(),
        cwd.path(),
        "00000000-0000-4000-8000-000000000000",
        "mock_history_echo",
    )
    .expect("spawn resumed TUI in PTY");

    let mut output = Vec::new();
    accept_new_workspace(&mut process, &mut output);
    receive_until(
        &process,
        &mut output,
        "Mock history users: mock_history_echo",
        Duration::from_secs(10),
        "the prompt did not go to a new conversation when the named one could not be resumed",
    );
    arm_idle_exit(&mut process, &mut output);
    let status = process.wait_for_exit(Duration::from_secs(5));
    process.close_io_and_join();
    assert_eq!(status.code(), Some(130), "TUI exited with {status}");
}

#[test]
fn a_message_typed_before_the_resumed_history_loads_runs_after_the_prompt() {
    let home = tempfile::tempdir().expect("temporary ORCA_HOME");
    let _recorded_in = record_a_conversation(home.path(), "pty resume seed");
    let cwd = tempfile::tempdir().expect("temporary workspace");
    let mut process =
        PtyProcess::spawn_resumed(home.path(), cwd.path(), "latest", "mock_history_echo")
            .expect("spawn resumed TUI in PTY");

    let mut output = Vec::new();
    receive_until(
        &process,
        &mut output,
        "review this workspace security boundary",
        Duration::from_secs(20),
        "resuming a conversation skipped the review of a new workspace",
    );
    // Accepting the workspace starts the resume, and the message is typed
    // with it, before the history can have loaded.
    process
        .write(b"\rheld message\r")
        .expect("accept the workspace and send a message");

    receive_until(
        &process,
        &mut output,
        "Mock history users: pty resume seed | mock_history_echo",
        Duration::from_secs(10),
        "the prompt did not run right after the resumed conversation",
    );
    // Both turns are over once the sentinel is on screen a second time (the
    // seed's answer is in the history), and the message is settled once its
    // line in the conversation is the only place that shows it: while its turn
    // runs, the queue strip shows it too.
    let deadline = Instant::now() + Duration::from_secs(10);
    let screen = loop {
        let screen = reconstruct_screen(&output);
        if screen.matches(ASSISTANT_SENTINEL).count() >= 2
            && screen.matches("held message").count() == 1
            && user_lines_showing(&screen, "held message") == 1
        {
            break screen;
        }
        assert!(
            Instant::now() < deadline,
            "the held message is not on screen once, after the answer to the prompt; reconstructed screen=\n{screen}"
        );
        if let Some(chunk) = process.receive_output(Duration::from_millis(250)) {
            output.extend_from_slice(&chunk);
        }
    };

    assert!(
        !screen.contains("pty resume seed | held message"),
        "the held message ran before the prompt; reconstructed screen=\n{screen}"
    );
    // It reads after the answer to the prompt, as a message sent during that
    // turn does, not above it.
    let answer = screen
        .find("Mock history users: pty resume seed | mock_history_echo")
        .expect("the answer to the prompt");
    let held = screen.find("held message").expect("the held message");
    assert!(
        answer < held,
        "the held message is above the answer to the prompt; reconstructed screen=\n{screen}"
    );

    arm_idle_exit(&mut process, &mut output);
    let status = process.wait_for_exit(Duration::from_secs(5));
    process.close_io_and_join();
    assert_eq!(status.code(), Some(130), "TUI exited with {status}");
}

#[test]
fn messages_typed_before_the_resumed_history_loads_run_in_the_order_they_were_typed() {
    let home = tempfile::tempdir().expect("temporary ORCA_HOME");
    let _recorded_in = record_a_conversation(home.path(), "pty resume seed");
    let cwd = tempfile::tempdir().expect("temporary workspace");
    let mut process =
        PtyProcess::spawn_resumed(home.path(), cwd.path(), "latest", "mock_history_echo")
            .expect("spawn resumed TUI in PTY");

    let mut output = Vec::new();
    receive_until(
        &process,
        &mut output,
        "review this workspace security boundary",
        Duration::from_secs(20),
        "resuming a conversation skipped the review of a new workspace",
    );
    // The second message asks the mock for the user messages it was given,
    // which is how the order they ran in shows.
    process
        .write(b"\rfirst held\rmock_history_echo\r")
        .expect("accept the workspace and send two messages");

    // The prompt on the command line comes first, as it asks the same; the
    // answer to the last message lists all of them.
    assert_screen_has_text(
        &process,
        &mut output,
        "Mock history users: pty resume seed | mock_history_echo | first held | mock_history_echo",
        "the messages did not run in the order they were typed, after the prompt",
    );
    arm_idle_exit(&mut process, &mut output);
    let status = process.wait_for_exit(Duration::from_secs(5));
    process.close_io_and_join();
    assert_eq!(status.code(), Some(130), "TUI exited with {status}");
}

#[test]
fn a_message_typed_before_a_conversation_that_cannot_be_resumed_is_not_held_for_good() {
    let home = tempfile::tempdir().expect("temporary ORCA_HOME");
    let cwd = tempfile::tempdir().expect("temporary workspace");
    // No prompt is given: the empty history that the failed resume sends is
    // all that tells the TUI that the message it holds may go.
    let mut process = PtyProcess::spawn_resumed_without_prompt(
        home.path(),
        cwd.path(),
        "00000000-0000-4000-8000-000000000000",
    )
    .expect("spawn resumed TUI in PTY");

    let mut output = Vec::new();
    receive_until(
        &process,
        &mut output,
        "review this workspace security boundary",
        Duration::from_secs(20),
        "resuming a conversation skipped the review of a new workspace",
    );
    process
        .write(b"\rmock_history_echo\r")
        .expect("accept the workspace and send a message");

    receive_until(
        &process,
        &mut output,
        "Mock history users: mock_history_echo",
        Duration::from_secs(10),
        "the message went to no conversation when the named one could not be resumed",
    );
    arm_idle_exit(&mut process, &mut output);
    let status = process.wait_for_exit(Duration::from_secs(5));
    process.close_io_and_join();
    assert_eq!(status.code(), Some(130), "TUI exited with {status}");
}

/// Records a conversation of one message, `prompt`, from a workspace of its
/// own that the run accepts first, and returns that workspace.
fn record_a_conversation(home: &std::path::Path, prompt: &str) -> tempfile::TempDir {
    let cwd = tempfile::tempdir().expect("temporary workspace");
    let mut process =
        PtyProcess::spawn_with_prompt(home, cwd.path(), prompt).expect("spawn TUI in PTY");
    let mut output = Vec::new();
    accept_new_workspace(&mut process, &mut output);
    receive_until(
        &process,
        &mut output,
        ASSISTANT_SENTINEL,
        Duration::from_secs(10),
        "the conversation to resume did not complete",
    );
    arm_idle_exit(&mut process, &mut output);
    let status = process.wait_for_exit(Duration::from_secs(5));
    process.close_io_and_join();
    assert_eq!(status.code(), Some(130), "TUI exited with {status}");
    cwd
}

#[test]
fn tui_quit_stops_an_mcp_server_that_is_still_starting() {
    let home = tempfile::tempdir().expect("temporary ORCA_HOME");
    let cwd = tempfile::tempdir().expect("temporary workspace");
    let fixture = tempfile::tempdir().expect("MCP fixture directory");
    let server = SlowMcpServer::configure(home.path(), fixture.path());
    let mut process =
        PtyProcess::spawn_without_prompt(home.path(), cwd.path()).expect("spawn TUI in PTY");

    let mut output = Vec::new();
    accept_new_workspace(&mut process, &mut output);
    let pid = server.wait_for_start();

    arm_idle_exit(&mut process, &mut output);
    let status = process.wait_for_exit(Duration::from_secs(5));
    process.close_io_and_join();
    assert_eq!(status.code(), Some(130), "TUI exited with {status}");
    assert_server_stops(&pid);
}

#[test]
fn tui_exit_from_first_run_setup_starts_no_mcp_server() {
    let home = tempfile::tempdir().expect("temporary ORCA_HOME");
    let cwd = tempfile::tempdir().expect("temporary workspace");
    let fixture = tempfile::tempdir().expect("MCP fixture directory");
    let server = SlowMcpServer::configure(home.path(), fixture.path());
    let mut process =
        PtyProcess::spawn_without_prompt(home.path(), cwd.path()).expect("spawn TUI in PTY");

    let mut output = Vec::new();
    receive_until(
        &process,
        &mut output,
        "review this workspace security boundary",
        Duration::from_secs(20),
        "TUI did not ask to review the new workspace",
    );
    // A server started with the TUI would have started by now.
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        assert_eq!(
            server.started(),
            Vec::<String>::new(),
            "an MCP server started before the workspace was accepted"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    process.write(b"e").expect("choose Exit");
    let status = process.wait_for_exit(Duration::from_secs(5));
    process.close_io_and_join();

    assert_eq!(status.code(), Some(0), "TUI exited with {status}");
    assert_eq!(
        server.started(),
        Vec::<String>::new(),
        "choosing Exit started an MCP server"
    );
}

#[test]
fn tui_sigterm_quits_like_exit_and_stops_mcp_servers() {
    let home = tempfile::tempdir().expect("temporary ORCA_HOME");
    let cwd = tempfile::tempdir().expect("temporary workspace");
    let fixture = tempfile::tempdir().expect("MCP fixture directory");
    let server = SlowMcpServer::configure(home.path(), fixture.path());
    let mut process =
        PtyProcess::spawn_without_prompt(home.path(), cwd.path()).expect("spawn TUI in PTY");

    let mut output = Vec::new();
    accept_new_workspace(&mut process, &mut output);
    let pid = server.wait_for_start();

    process.drain_output(&mut output);
    let signalled_at = output.len();
    process.signal(libc::SIGTERM);
    let status = process.wait_for_exit(Duration::from_secs(5));
    process.close_io_and_join();
    process.drain_output(&mut output);

    assert_eq!(status.code(), Some(143), "TUI exited with {status}");
    assert_server_stops(&pid);
    assert_restores_the_terminal(&output[signalled_at..]);
}

#[test]
fn tui_sighup_with_the_terminal_gone_still_stops_mcp_servers() {
    let home = tempfile::tempdir().expect("temporary ORCA_HOME");
    let cwd = tempfile::tempdir().expect("temporary workspace");
    let fixture = tempfile::tempdir().expect("MCP fixture directory");
    let server = SlowMcpServer::configure(home.path(), fixture.path());
    let mut process =
        PtyProcess::spawn_without_prompt(home.path(), cwd.path()).expect("spawn TUI in PTY");

    let mut output = Vec::new();
    accept_new_workspace(&mut process, &mut output);
    let pid = server.wait_for_start();

    // The terminal goes first, as when its window closes: the TUI's reads
    // end and its writes fail before the hangup arrives.
    process.hang_up();
    process.signal(libc::SIGHUP);
    let status = process.wait_for_exit(Duration::from_secs(5));

    assert_eq!(status.code(), Some(129), "TUI exited with {status}");
    assert_server_stops(&pid);
}

/// A signal sent to a process group can reach the TUI twice, a millisecond
/// or so apart: a wrapper script such as the npm launcher, in the same group,
/// passes on what it got, and a shell passes on its terminal's SIGHUP. The
/// copy is the same signal, and must not cut the quit short.
#[test]
fn tui_sigterm_delivered_twice_still_stops_mcp_servers() {
    let home = tempfile::tempdir().expect("temporary ORCA_HOME");
    let cwd = tempfile::tempdir().expect("temporary workspace");
    let fixture = tempfile::tempdir().expect("MCP fixture directory");
    let server = SlowMcpServer::configure(home.path(), fixture.path());
    let mut process =
        PtyProcess::spawn_without_prompt(home.path(), cwd.path()).expect("spawn TUI in PTY");

    let mut output = Vec::new();
    accept_new_workspace(&mut process, &mut output);
    let pid = server.wait_for_start();

    process.drain_output(&mut output);
    let signalled_at = output.len();
    process.signal(libc::SIGTERM);
    std::thread::sleep(Duration::from_millis(1));
    process.signal(libc::SIGTERM);
    let status = process.wait_for_exit(Duration::from_secs(5));
    process.close_io_and_join();
    process.drain_output(&mut output);

    assert_eq!(status.code(), Some(143), "TUI exited with {status}");
    assert!(
        !contains_rendered_text(&output[signalled_at..], "received a second"),
        "the copy cut the quit short; output={}",
        String::from_utf8_lossy(&output[signalled_at..])
    );
    assert_server_stops(&pid);
}

/// The same with the terminal gone, as when a window closes and the shell
/// passes its SIGHUP on.
#[test]
fn tui_sighup_delivered_twice_with_the_terminal_gone_still_stops_mcp_servers() {
    let home = tempfile::tempdir().expect("temporary ORCA_HOME");
    let cwd = tempfile::tempdir().expect("temporary workspace");
    let fixture = tempfile::tempdir().expect("MCP fixture directory");
    let server = SlowMcpServer::configure(home.path(), fixture.path());
    let mut process =
        PtyProcess::spawn_without_prompt(home.path(), cwd.path()).expect("spawn TUI in PTY");

    let mut output = Vec::new();
    accept_new_workspace(&mut process, &mut output);
    let pid = server.wait_for_start();

    process.hang_up();
    process.signal(libc::SIGHUP);
    std::thread::sleep(Duration::from_millis(1));
    process.signal(libc::SIGHUP);
    let status = process.wait_for_exit(Duration::from_secs(5));

    assert_eq!(status.code(), Some(129), "TUI exited with {status}");
    assert_server_stops(&pid);
}

/// A terminal can hang up with no SIGHUP to say so (it is not the TUI's
/// controlling terminal, as here), and when one comes, the TUI may see its
/// input end first. Either way it quits as on SIGHUP.
#[test]
fn tui_whose_terminal_hangs_up_quits_as_on_sighup() {
    let home = tempfile::tempdir().expect("temporary ORCA_HOME");
    let cwd = tempfile::tempdir().expect("temporary workspace");
    let fixture = tempfile::tempdir().expect("MCP fixture directory");
    let server = SlowMcpServer::configure(home.path(), fixture.path());
    let mut process =
        PtyProcess::spawn_without_prompt(home.path(), cwd.path()).expect("spawn TUI in PTY");

    let mut output = Vec::new();
    accept_new_workspace(&mut process, &mut output);
    let pid = server.wait_for_start();

    process.hang_up();
    let status = process.wait_for_exit(Duration::from_secs(5));

    assert_eq!(status.code(), Some(129), "TUI exited with {status}");
    assert_server_stops(&pid);
}

#[test]
fn tui_sigterm_during_a_turn_interrupts_it() {
    let home = tempfile::tempdir().expect("temporary ORCA_HOME");
    let cwd = tempfile::tempdir().expect("temporary workspace");
    let mut process =
        PtyProcess::spawn_with_prompt(home.path(), cwd.path(), "mock_stream_delay_ms 10000")
            .expect("spawn TUI in PTY");

    let mut output = Vec::new();
    accept_new_workspace(&mut process, &mut output);
    receive_until(
        &process,
        &mut output,
        "Running 0s",
        Duration::from_secs(20),
        "TUI did not render the active turn before the signal",
    );
    // The screen says so as soon as the message is sent: wait for the
    // runtime to run the turn too.
    let turn = wait_for_a_turn_record(home.path(), "\"type\":\"turn_started\"");
    process.drain_output(&mut output);
    let signalled_at = output.len();
    process.signal(libc::SIGTERM);
    let status = process.wait_for_exit(Duration::from_secs(5));
    process.close_io_and_join();
    process.drain_output(&mut output);

    assert_eq!(status.code(), Some(143), "TUI exited with {status}");
    let record = std::fs::read_to_string(&turn).expect("the turn's record");
    let terminal = record
        .lines()
        .find(|line| line.contains("\"type\":\"operation_terminal\""))
        .unwrap_or_else(|| panic!("the turn ended without a terminal record: {record}"));
    assert!(
        terminal.contains("\"terminal\":{\"cancelled\""),
        "the turn was not interrupted: {terminal}"
    );
    // The terminal is still there: the exit tells how to resume.
    assert!(
        contains_rendered_text(&output[signalled_at..], "orca --resume"),
        "no resume hint after the signal; output={}",
        String::from_utf8_lossy(&output[signalled_at..])
    );
}

/// Waits for the record of a turn under `home` to hold `needle`, and
/// returns its path.
fn wait_for_a_turn_record(home: &std::path::Path, needle: &str) -> std::path::PathBuf {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let found = std::fs::read_dir(home.join("operations"))
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| std::fs::read_to_string(path).is_ok_and(|record| record.contains(needle)));
        if let Some(path) = found {
            return path;
        }
        assert!(
            Instant::now() < deadline,
            "no turn record holds {needle} within 10 s"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn tui_sigterm_during_the_workspace_review_exits_cleanly() {
    let home = tempfile::tempdir().expect("temporary ORCA_HOME");
    let cwd = tempfile::tempdir().expect("temporary workspace");
    let fixture = tempfile::tempdir().expect("MCP fixture directory");
    let server = SlowMcpServer::configure(home.path(), fixture.path());
    let mut process =
        PtyProcess::spawn_without_prompt(home.path(), cwd.path()).expect("spawn TUI in PTY");

    let mut output = Vec::new();
    receive_until(
        &process,
        &mut output,
        "review this workspace security boundary",
        Duration::from_secs(20),
        "TUI did not ask to review the new workspace",
    );
    process.drain_output(&mut output);
    let signalled_at = output.len();
    process.signal(libc::SIGTERM);
    let status = process.wait_for_exit(Duration::from_secs(5));
    process.close_io_and_join();
    process.drain_output(&mut output);

    assert_eq!(status.code(), Some(143), "TUI exited with {status}");
    assert_restores_the_terminal(&output[signalled_at..]);
    assert_eq!(
        server.started(),
        Vec::<String>::new(),
        "a signal during the workspace review started an MCP server"
    );
}

/// Asserts that the MCP server `pid` is gone within a second of the TUI's
/// exit: still starting, it would have read nothing for 30 s.
fn assert_server_stops(pid: &str) {
    let deadline = Instant::now() + Duration::from_secs(1);
    while process_is_alive(pid) {
        assert!(
            Instant::now() < deadline,
            "the MCP server {pid} outlived the TUI"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Asserts that `output`, what the TUI wrote after a signal, puts the
/// terminal back as an ordinary exit does: bracketed paste off, among the
/// rest.
fn assert_restores_the_terminal(output: &[u8]) {
    const BRACKETED_PASTE_OFF: &[u8] = b"\x1b[?2004l";
    assert!(
        output
            .windows(BRACKETED_PASTE_OFF.len())
            .any(|window| window == BRACKETED_PASTE_OFF),
        "the TUI did not restore the terminal; output={}",
        String::from_utf8_lossy(output)
    );
}

/// A stdio MCP server, in the user config of an ORCA_HOME, that adds its
/// process id to a file as it starts, and reads nothing for 30 seconds:
/// until then it is starting. Dropping it kills the server's process group,
/// should a failed test have left it running.
struct SlowMcpServer {
    pids: std::path::PathBuf,
}

impl SlowMcpServer {
    fn configure(home: &std::path::Path, dir: &std::path::Path) -> Self {
        let script = dir.join("slow.sh");
        std::fs::write(
            &script,
            "printf '%s\\n' \"$$\" >> \"$1/pids\"\nsleep 30\nwhile IFS= read -r line; do :; done\n",
        )
        .expect("write the MCP server");
        std::fs::write(
            home.join("config.toml"),
            format!(
                "[[mcp_servers]]\nname = \"slow\"\ntransport = \"stdio\"\ncommand = \"/bin/sh\"\nargs = [{:?}, {:?}]\n",
                script.display().to_string(),
                dir.display().to_string(),
            ),
        )
        .expect("configure the MCP server");
        Self {
            pids: dir.join("pids"),
        }
    }

    fn started(&self) -> Vec<String> {
        std::fs::read_to_string(&self.pids)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// The process id of the server's first start, once it has started.
    fn wait_for_start(&self) -> String {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(pid) = self.started().into_iter().next() {
                return pid;
            }
            assert!(Instant::now() < deadline, "the MCP server never started");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for SlowMcpServer {
    fn drop(&mut self) {
        for pid in self.started() {
            let _ = Command::new("kill")
                .args(["-KILL", "--", &format!("-{pid}")])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

fn process_is_alive(pid: &str) -> bool {
    Command::new("kill")
        .args(["-0", pid])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn assert_screen_shows(process: &PtyProcess, output: &mut Vec<u8>, expected: &str, failure: &str) {
    // PTY contracts run in parallel and the mock child may not publish its
    // first activity frame until other test binaries release the CPU. Wait for
    // the visible state boundary instead of treating scheduler latency as a
    // missing projection.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if screen_contains(output, expected) {
            return;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            panic!(
                "{failure}; reconstructed screen=\n{}",
                reconstruct_screen(output)
            );
        }
        if let Some(chunk) = process.receive_output(remaining.min(Duration::from_millis(250))) {
            output.extend_from_slice(&chunk);
        }
    }
}

/// Like `assert_screen_shows`, but `expected` has to be on the screen as it
/// is, not in pieces somewhere in it: where text comes in an order is what is
/// checked.
fn assert_screen_has_text(
    process: &PtyProcess,
    output: &mut Vec<u8>,
    expected: &str,
    failure: &str,
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if reconstruct_screen(output).contains(expected) {
            return;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            panic!(
                "{failure}; reconstructed screen=\n{}",
                reconstruct_screen(output)
            );
        }
        if let Some(chunk) = process.receive_output(remaining.min(Duration::from_millis(250))) {
            output.extend_from_slice(&chunk);
        }
    }
}

fn assert_screen_shows_wrapped_token(
    process: &PtyProcess,
    output: &mut Vec<u8>,
    expected: &str,
    failure: &str,
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let compact: String = reconstruct_screen(output)
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect();
        if compact.contains(expected) {
            return;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            panic!(
                "{failure}; reconstructed screen=\n{}",
                reconstruct_screen(output)
            );
        }
        if let Some(chunk) = process.receive_output(remaining.min(Duration::from_millis(250))) {
            output.extend_from_slice(&chunk);
        }
    }
}

/// How many of the lines of `screen` are a message the user sent that shows
/// `text`: the conversation marks those with `›`.
fn user_lines_showing(screen: &str, text: &str) -> usize {
    screen
        .lines()
        .filter(|line| line.trim_start().starts_with('›') && line.contains(text))
        .count()
}

fn screen_contains(output: &[u8], expected: &str) -> bool {
    let screen = reconstruct_screen(output);
    let mut cursor = 0;
    for token in expected.split_whitespace() {
        let Some(offset) = screen[cursor..].find(token) else {
            return false;
        };
        cursor += offset + token.len();
    }
    true
}

// Minimal ANSI interpreter: honours cursor positioning (CSI row;col H) and the
// erase-screen (CSI 2 J) sequence, dropping other CSI/OSC controls. Enough to
// materialize the visible grid our TUI paints.
fn reconstruct_screen(output: &[u8]) -> String {
    use std::collections::BTreeMap;
    let text = String::from_utf8_lossy(output);
    let mut grid: BTreeMap<(usize, usize), char> = BTreeMap::new();
    let (mut row, mut col) = (1usize, 1usize);
    let bytes: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        let ch = bytes[i];
        if ch == '\u{1b}' {
            // OSC: ESC ] ... (BEL | ESC \)
            if bytes.get(i + 1) == Some(&']') {
                i += 2;
                while i < bytes.len() && bytes[i] != '\u{07}' && bytes[i] != '\u{1b}' {
                    i += 1;
                }
                if bytes.get(i) == Some(&'\u{1b}') {
                    i += 1;
                }
                i += 1;
                continue;
            }
            // CSI: ESC [ params final
            if bytes.get(i + 1) == Some(&'[') {
                let mut j = i + 2;
                let mut params = String::new();
                while j < bytes.len() && !bytes[j].is_ascii_alphabetic() {
                    params.push(bytes[j]);
                    j += 1;
                }
                let final_byte = bytes.get(j).copied().unwrap_or(' ');
                match final_byte {
                    'H' | 'f' => {
                        let mut parts = params.split(';');
                        row = parts
                            .next()
                            .and_then(|p| p.parse().ok())
                            .unwrap_or(1)
                            .max(1);
                        col = parts
                            .next()
                            .and_then(|p| p.parse().ok())
                            .unwrap_or(1)
                            .max(1);
                    }
                    'J' => {
                        if params == "2" {
                            grid.clear();
                        }
                    }
                    _ => {}
                }
                i = j + 1;
                continue;
            }
            i += 2;
            continue;
        }
        match ch {
            '\n' => {
                row += 1;
                col = 1;
            }
            '\r' => col = 1,
            _ => {
                grid.insert((row, col), ch);
                col += 1;
            }
        }
        i += 1;
    }
    let max_row = grid.keys().map(|(r, _)| *r).max().unwrap_or(0);
    let mut lines = Vec::new();
    for r in 1..=max_row {
        let cols: Vec<usize> = grid
            .range((r, 0)..(r + 1, 0))
            .map(|((_, c), _)| *c)
            .collect();
        let max_col = cols.iter().copied().max().unwrap_or(0);
        let line: String = (1..=max_col)
            .map(|c| grid.get(&(r, c)).copied().unwrap_or(' '))
            .collect();
        lines.push(line.trim_end().to_string());
    }
    lines.join("\n")
}

struct PtyProcess {
    child: Option<Child>,
    writer: Option<File>,
    reader: Option<JoinHandle<()>>,
    output_rx: Receiver<Vec<u8>>,
    /// Stops the reader, which then closes its end of the terminal.
    stop_reading: Arc<AtomicBool>,
}

impl PtyProcess {
    fn spawn(home: &std::path::Path, cwd: &std::path::Path) -> io::Result<Self> {
        Self::spawn_with_prompt(home, cwd, PROMPT)
    }

    fn spawn_with_prompt(
        home: &std::path::Path,
        cwd: &std::path::Path,
        prompt: &str,
    ) -> io::Result<Self> {
        Self::spawn_with_history_and_model(home, cwd, None, None, prompt)
    }

    fn spawn_with_model_and_prompt(
        home: &std::path::Path,
        cwd: &std::path::Path,
        model: &str,
        prompt: &str,
    ) -> io::Result<Self> {
        Self::spawn_with_history_and_model(home, cwd, None, Some(model), prompt)
    }

    fn spawn_resumed(
        home: &std::path::Path,
        cwd: &std::path::Path,
        selector: &str,
        prompt: &str,
    ) -> io::Result<Self> {
        Self::spawn_with_history(home, cwd, Some(selector), prompt)
    }

    /// `orca --resume selector` in `cwd` with no prompt.
    fn spawn_resumed_without_prompt(
        home: &std::path::Path,
        cwd: &std::path::Path,
        selector: &str,
    ) -> io::Result<Self> {
        let mut command = Command::new(env!("CARGO_BIN_EXE_orca"));
        command.args(["--provider", "mock", "--cwd"]).arg(cwd);
        command.args(["--resume", selector]);
        Self::spawn_command(command, home)
    }

    fn spawn_with_history(
        home: &std::path::Path,
        cwd: &std::path::Path,
        resume: Option<&str>,
        prompt: &str,
    ) -> io::Result<Self> {
        Self::spawn_with_history_and_model(home, cwd, resume, None, prompt)
    }

    fn spawn_with_history_and_model(
        home: &std::path::Path,
        cwd: &std::path::Path,
        resume: Option<&str>,
        model: Option<&str>,
        prompt: &str,
    ) -> io::Result<Self> {
        let mut command = Command::new(env!("CARGO_BIN_EXE_orca"));
        command.args(["--provider", "mock", "--cwd"]).arg(cwd);
        if let Some(model) = model {
            command.args(["--model", model]);
        }
        if let Some(selector) = resume {
            command.args(["--resume", selector]);
        }
        command.arg(prompt);
        Self::spawn_command(command, home)
    }

    /// `orca` in `cwd` with no prompt, as a user opens it.
    fn spawn_without_prompt(home: &std::path::Path, cwd: &std::path::Path) -> io::Result<Self> {
        let mut command = Command::new(env!("CARGO_BIN_EXE_orca"));
        command.args(["--provider", "mock", "--cwd"]).arg(cwd);
        Self::spawn_command(command, home)
    }

    /// Runs `command` on a terminal of its own, with `home` as its ORCA_HOME.
    fn spawn_command(mut command: Command, home: &std::path::Path) -> io::Result<Self> {
        let (master, slave) = open_pty(120, 40)?;
        let stdout = duplicate_fd(&slave)?;
        let stderr = duplicate_fd(&slave)?;
        let writer = File::from(duplicate_fd(&master)?);
        let mut terminal_reader = File::from(master);
        let stdin = File::from(slave);

        let child = command
            .env("ORCA_HOME", home)
            .env("ORCA_API_KEY", "pty-test-key")
            .env("TERM", "xterm-256color")
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::from(File::from(stdout)))
            .stderr(Stdio::from(File::from(stderr)))
            .spawn()?;

        let (output_tx, output_rx) = mpsc::channel();
        let stop_reading = Arc::new(AtomicBool::new(false));
        let reader = std::thread::spawn({
            let stop_reading = Arc::clone(&stop_reading);
            move || {
                let mut buffer = [0_u8; 4096];
                // It waits for output a little at a time, so that `hang_up`
                // can stop it while the TUI still runs.
                while !stop_reading.load(Ordering::SeqCst) {
                    if !readable(&terminal_reader, Duration::from_millis(25)) {
                        continue;
                    }
                    match terminal_reader.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(read) => {
                            if output_tx.send(buffer[..read].to_vec()).is_err() {
                                break;
                            }
                        }
                        Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
                        Err(error) => panic!("read TUI PTY: {error}"),
                    }
                }
            }
        });

        Ok(Self {
            child: Some(child),
            writer: Some(writer),
            reader: Some(reader),
            output_rx,
            stop_reading,
        })
    }

    fn pid(&self) -> u32 {
        self.child.as_ref().expect("PTY child remains owned").id()
    }

    /// Sends `signal` to the TUI, as `kill` does.
    fn signal(&self, signal: libc::c_int) {
        let pid = libc::pid_t::try_from(self.pid()).expect("the TUI's pid");
        assert_eq!(
            unsafe { libc::kill(pid, signal) },
            0,
            "send signal {signal} to the TUI"
        );
    }

    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "PTY writer is closed"))?;
        writer.write_all(bytes)?;
        writer.flush()
    }

    fn receive_output(&self, timeout: Duration) -> Option<Vec<u8>> {
        self.output_rx.recv_timeout(timeout).ok()
    }

    fn drain_output(&self, output: &mut Vec<u8>) {
        output.extend(self.output_rx.try_iter().flatten());
    }

    fn wait_for_exit(&mut self, timeout: Duration) -> ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self
                .child
                .as_mut()
                .expect("PTY child remains owned")
                .try_wait()
                .expect("poll TUI process")
            {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "TUI did not exit within {timeout:?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child
            .as_mut()
            .expect("PTY child remains owned")
            .try_wait()
    }

    fn close_io_and_join(&mut self) {
        self.writer.take();
        if let Some(reader) = self.reader.take() {
            reader.join().expect("join PTY reader");
        }
    }

    /// Closes the terminal from this end while the TUI runs, as closing its
    /// window does: stops reading it and closes every descriptor of the
    /// PTY's master, so the TUI's reads end and its writes fail. What it
    /// writes from then on is lost.
    fn hang_up(&mut self) {
        self.stop_reading.store(true, Ordering::SeqCst);
        self.writer.take();
        if let Some(reader) = self.reader.take() {
            reader.join().expect("join PTY reader");
        }
    }
}

impl Drop for PtyProcess {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            match child.try_wait() {
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
        }
        self.child.take();
        self.writer.take();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn arm_idle_exit(process: &mut PtyProcess, output: &mut Vec<u8>) {
    cancel_running_turn_and_exit(process, output);
}

fn cancel_running_turn_and_exit(process: &mut PtyProcess, output: &mut Vec<u8>) {
    const IDLE_EXIT_NOTICE: &str = "Press Ctrl+C again to quit.";

    process.drain_output(output);
    let notice_start = output.len();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if send_ctrl_c_or_observe_idle_exit(process, "interrupt the running turn or arm idle exit")
        {
            return;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "TUI did not settle the cancelled turn within 5s; output={}",
            String::from_utf8_lossy(output)
        );
        if let Some(chunk) = process.receive_output(remaining.min(Duration::from_millis(250))) {
            output.extend_from_slice(&chunk);
        }
        process.drain_output(output);
        if let Some(status) = process.try_wait().expect("poll cancelled TUI exit") {
            assert_eq!(
                status.code(),
                Some(130),
                "cancelled TUI exited with {status}"
            );
            return;
        }
        if contains_rendered_text(&output[notice_start..], IDLE_EXIT_NOTICE) {
            break;
        }
    }
    await_idle_ctrl_c_exit(process);
}

fn send_ctrl_c_or_observe_idle_exit(process: &mut PtyProcess, action: &str) -> bool {
    match process.write(&[0x03]) {
        Ok(()) => false,
        Err(error) => {
            let status = process.wait_for_exit(Duration::from_secs(1));
            assert_eq!(
                status.code(),
                Some(130),
                "{action} failed with {error}; TUI exited with {status}"
            );
            true
        }
    }
}

fn await_idle_ctrl_c_exit(process: &mut PtyProcess) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match process.write(&[0x03]) {
            Ok(()) => {}
            Err(error) => {
                if let Some(status) = process.try_wait().expect("poll idle Ctrl-C exit") {
                    assert_eq!(
                        status.code(),
                        Some(130),
                        "idle Ctrl-C closed the PTY with {error}, but TUI exited with {status}"
                    );
                    return;
                }
            }
        }
        if let Some(status) = process.try_wait().expect("poll idle Ctrl-C exit") {
            assert_eq!(status.code(), Some(130), "TUI exited with {status}");
            return;
        }
        assert!(
            Instant::now() < deadline,
            "TUI did not consume idle Ctrl-C within 2s"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Accepts the review of a new workspace. A TUI launched in a fresh
/// ORCA_HOME shows it before it runs anything, a prompt given on the command
/// line included.
fn accept_new_workspace(process: &mut PtyProcess, output: &mut Vec<u8>) {
    receive_until(
        process,
        output,
        "review this workspace security boundary",
        Duration::from_secs(20),
        "TUI did not ask to review the new workspace",
    );
    process.write(b"\r").expect("trust the workspace");
}

fn receive_until(
    process: &PtyProcess,
    output: &mut Vec<u8>,
    expected: &str,
    timeout: Duration,
    failure: &str,
) {
    let deadline = Instant::now() + timeout;
    while !contains_rendered_text(output, expected) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "{failure}; output={}",
            String::from_utf8_lossy(output)
        );
        if let Some(chunk) = process.receive_output(remaining.min(Duration::from_millis(250))) {
            output.extend_from_slice(&chunk);
        }
    }
}

fn receive_until_after(
    process: &PtyProcess,
    output: &mut Vec<u8>,
    expected: &str,
    start: usize,
    timeout: Duration,
    failure: &str,
) {
    let deadline = Instant::now() + timeout;
    while !contains_rendered_text(&output[start..], expected) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "{failure}; output={}",
            String::from_utf8_lossy(output)
        );
        if let Some(chunk) = process.receive_output(remaining.min(Duration::from_millis(250))) {
            output.extend_from_slice(&chunk);
        }
    }
}

fn contains_rendered_text(output: &[u8], expected: &str) -> bool {
    let rendered = String::from_utf8_lossy(output);
    let mut cursor = 0;
    for token in expected.split_whitespace() {
        let Some(offset) = rendered[cursor..].find(token) else {
            return false;
        };
        cursor += offset + token.len();
    }
    true
}

fn count_history_files(root: &std::path::Path) -> usize {
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| {
            let path = entry.path();
            if path.is_dir() {
                count_history_files(&path)
            } else if path.extension().is_some_and(|ext| ext == "jsonl") {
                1
            } else {
                0
            }
        })
        .sum()
}

fn open_pty(columns: u16, rows: u16) -> io::Result<(OwnedFd, OwnedFd)> {
    let mut master = -1;
    let mut slave = -1;
    let mut size = libc::winsize {
        ws_row: rows,
        ws_col: columns,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let result = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    // Tests spawn their TUIs in parallel: an inheritable PTY would leak into
    // every other test's child, and a detached subagent worker outliving that
    // child would hold this terminal open so its reader never saw EOF. Keep
    // close-on-exec duplicates and let the inheritable originals close; each
    // child still gets its own terminal, which spawning puts on fds 0-2.
    Ok((duplicate_fd(&master)?, duplicate_fd(&slave)?))
}

/// Whether `file` has something to read, or has reached its end, within
/// `timeout`.
fn readable(file: &File, timeout: Duration) -> bool {
    let mut poll = libc::pollfd {
        fd: file.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let timeout = libc::c_int::try_from(timeout.as_millis()).unwrap_or(libc::c_int::MAX);
    unsafe { libc::poll(&mut poll, 1, timeout) > 0 }
}

fn duplicate_fd(fd: &OwnedFd) -> io::Result<OwnedFd> {
    // `try_clone` duplicates with close-on-exec.
    fd.try_clone()
}
