# A background command's end reaches the model in the same turn

Date: 2026-11-27. Size: small project.

## Intent

The system prompt tells the model "Long-running commands notify you when they finish." Three things make that untrue.

1. **The notice waits for the next turn.** It is injected only when the next thread turn starts (`drain_terminal_notifications`, the one production call in `crates/orca-runtime/src/controller.rs`).
   - `orca exec` runs one turn, so a build or test that outlives `bash`'s 10 s yield is never announced.
   - In the TUI, a notice waits for the user's next message.
   - Subagent results, by contrast, are delivered at every settled model boundary (`deliver_child_results` in `runtime_turn_loop.rs`).
2. **The notice holds the first 8 KiB of output.** The `bash` call already returned that part; errors are usually printed last. `queue_completion` reads from offset 0 (`terminal_service.rs`).
3. **Commands the model already dealt with are announced again.** Only a read at the default cursor marks a completion as seen. `task_wait` and reads at an explicit offset do not, and `task_stop` queues a "stopped" notice for a stop the model asked for.

DeepSeek's harness handles this the same way the fixes below do:
- a finished job's notice goes to the running turn at its next step;
- only output the model has not read is included;
- nothing is sent when the agent already collected the end through a wait, or stopped the job itself.

## Goals

1. **Delivery inside the turn.** The turn loop delivers queued shell completions at the same settled model boundary where it delivers child results, and only when no tool calls are open.
   - Each notice is appended as a system message and recorded the way child results are, so that it survives a resume and is never appended twice.
   - Delivery at turn start stays as it is, for notices that arrive while no turn runs.
2. **The notice carries the end of the output.** It holds up to the last 8 KiB, not the first.
   - When output was left out, the notice says how many bytes came before and how to read them: `task_read_output` with that task id and a cursor.
   - The status, exit code and existing wording stay.
3. **A command the model already dealt with is not announced.** The completion is marked observed whenever a tool result has told the model the command ended:
   - `task_wait` returning a final status;
   - `task_read_output` at any cursor returning a final status;
   - `task_stop` called by the model.
   A queued notice for it is withdrawn.
4. **The system prompt line matches what happens.** In `crates/orca-runtime/src/system_prompt.rs` it becomes, in substance: "A command left running in the background is reported to you when it finishes: at your next step if this turn is still running, otherwise when the next turn starts. When you need its result before you continue or end your turn, use `task_wait`." The prompt stays within its existing size ceiling and tests.

## Non-goals

- No new turn is started for a notice that arrives while the TUI is idle.
- The turn does not wait for background commands before ending. A command left running on purpose, such as a server, must not hold the turn.

## Constraints

- Notices are system messages, as today. No new event type is added.
- The prompt's size tests and budget constants (`system_prompt.rs` tests) must pass. If a pinned prefix size moves, update the pin in the same commit and say so.
