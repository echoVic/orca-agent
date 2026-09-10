use std::fs;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use orca_runtime::workflow::host::{HostEvent, WorkflowHost, WorkflowHostIpcPaths};
use tempfile::tempdir;

// Exact JavaScript bytes are the released workflow-host protocol fixture.
const WORKFLOW_HOST_SCRIPT: &str = include_str!("../crates/orca-runtime/src/workflow/host.mjs");

#[test]
fn host_emits_phase_and_agent_call_events() {
    if !WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'host-test', description: 'Host test', phases: ['scan'] };\nconst result = await phase('scan', async () => agent('inspect repo', { description: 'scan repo' }));\nexport default result;",
    )
    .unwrap();

    let events = WorkflowHost::run_collecting_events(&script, serde_json::json!({"x": 1})).unwrap();

    assert!(
        events
            .iter()
            .any(|event| matches!(event, HostEvent::PhaseStarted { name } if name == "scan"))
    );
    assert!(events.iter().any(
        |event| matches!(event, HostEvent::AgentCall { prompt, .. } if prompt == "inspect repo")
    ));
}

#[test]
fn host_phase_marker_applies_to_following_agents_until_changed() {
    if !WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'marker-test', description: 'Marker phase test', phases: ['scan', 'review'] };\nphase('scan');\nawait agent('inspect repo');\nphase('review');\nawait agent('review findings');\nexport default 'done';",
    )
    .unwrap();

    let events = WorkflowHost::run_collecting_events(&script, serde_json::json!(null)).unwrap();

    assert!(events.iter().any(|event| {
        matches!(
            event,
            HostEvent::AgentCall { prompt, phase, .. }
                if prompt == "inspect repo" && phase.as_deref() == Some("scan")
        )
    }));
    assert!(events.iter().any(|event| {
        matches!(
            event,
            HostEvent::AgentCall { prompt, phase, .. }
                if prompt == "review findings" && phase.as_deref() == Some("review")
        )
    }));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, HostEvent::PhaseCompleted { name } if name == "scan"))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, HostEvent::PhaseCompleted { name } if name == "review"))
    );
}

#[test]
fn host_parallel_routes_out_of_order_agent_results_by_call_id() {
    if !WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'parallel-host-test', description: 'Parallel host test', phases: [] };\nconst results = await parallel([agent('slow'), agent('fast')]);\nexport default results.map(item => item.prompt).join(',');",
    )
    .unwrap();

    let events =
        WorkflowHost::run_collecting_events_with_agent(&script, serde_json::json!(null), |call| {
            if call.prompt == "slow" {
                thread::sleep(Duration::from_millis(150));
            }
            Ok(orca_runtime::workflow::host::HostCommand::AgentResult {
                call_id: call.call_id.clone(),
                result: serde_json::json!({
                    "prompt": call.prompt,
                }),
            })
        })
        .unwrap();

    assert!(events.iter().any(|event| {
        matches!(
            event,
            HostEvent::WorkflowCompleted { result }
                if result.as_str() == Some("slow,fast")
        )
    }));
}

#[test]
fn host_parallel_fans_out_eight_agents_and_preserves_result_order() {
    if !WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'eight-agent-host-test', description: 'Eight agent host test', phases: ['research'] };\n\
         const prompts = Array.from({ length: 8 }, (_, index) => `agent-${index + 1}`);\n\
         const results = await phase('research', async () => parallel(prompts.map(prompt => agent(prompt))));\n\
         export default results.map(item => item.prompt);",
    )
    .unwrap();

    let events =
        WorkflowHost::run_collecting_events_with_agent(&script, serde_json::json!(null), |call| {
            let index = call
                .prompt
                .strip_prefix("agent-")
                .and_then(|suffix| suffix.parse::<u64>().ok())
                .unwrap_or(0);
            thread::sleep(Duration::from_millis((9 - index) * 10));
            Ok(orca_runtime::workflow::host::HostCommand::AgentResult {
                call_id: call.call_id.clone(),
                result: serde_json::json!({
                    "prompt": call.prompt,
                }),
            })
        })
        .unwrap();

    let research_calls = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                HostEvent::AgentCall { phase, .. } if phase.as_deref() == Some("research")
            )
        })
        .count();
    assert_eq!(research_calls, 8);
    assert!(events.iter().any(|event| {
        matches!(
            event,
            HostEvent::WorkflowCompleted { result }
                if result.as_array().map(|items| items.iter().map(|item| item.as_str().unwrap_or_default()).collect::<Vec<_>>())
                    == Some(vec!["agent-1", "agent-2", "agent-3", "agent-4", "agent-5", "agent-6", "agent-7", "agent-8"])
        )
    }));
}

#[test]
fn host_can_conditionally_spawn_agents_from_args() {
    if !WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'dynamic-host-test', description: 'Dynamic host test', phases: ['fanout'] };\n\
         const prompts = args.enabled ? args.prompts : [];\n\
         const results = await phase('fanout', async () => parallel(prompts.map((prompt) => agent(prompt))));\n\
         export default results.map(item => item.prompt);",
    )
    .unwrap();

    let events = WorkflowHost::run_collecting_events(
        &script,
        serde_json::json!({
            "enabled": true,
            "prompts": ["scan", "review", "docs"]
        }),
    )
    .unwrap();

    let prompts = events
        .iter()
        .filter_map(|event| match event {
            HostEvent::AgentCall { prompt, phase, .. } if phase.as_deref() == Some("fanout") => {
                Some(prompt.as_str())
            }
            _ => None,
        })
        .collect::<Vec<_>>();

    assert_eq!(prompts, vec!["scan", "review", "docs"]);
    assert!(events.iter().any(|event| {
        matches!(
            event,
            HostEvent::WorkflowCompleted { result }
                if result.as_array().map(|items| items.len()) == Some(3)
        )
    }));
}

#[test]
fn host_message_channel_passes_agent_findings_to_later_agents() {
    if !WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'message-channel-test', description: 'Message channel test', phases: ['scan', 'review'] };\n\
         const scan = await phase('scan', async () => agent('inspect api'));\n\
         sendMessage('findings', { prompt: scan.prompt, severity: 'high' }, { from: 'scanner' });\n\
         const messages = readMessages('findings');\n\
         const review = await phase('review', async () => agent(`review ${messages[0].from} ${messages[0].message.severity} ${messages[0].message.prompt}`));\n\
         const cleared = clearMessages('findings');\n\
         export default { review, messageCount: messages.length, cleared, remaining: readMessages('findings').length };",
    )
    .unwrap();

    let events = WorkflowHost::run_collecting_events(&script, serde_json::json!(null)).unwrap();

    assert!(events.iter().any(|event| {
        matches!(
            event,
            HostEvent::AgentCall { prompt, phase, .. }
                if prompt == "review scanner high inspect api" && phase.as_deref() == Some("review")
        )
    }));
    assert!(events.iter().any(|event| {
        matches!(
            event,
            HostEvent::WorkflowCompleted { result }
                if result["messageCount"] == 1 && result["cleared"] == 1 && result["remaining"] == 0
        )
    }));
}

#[test]
fn host_task_list_distributes_shared_work_to_parallel_agents() {
    if !WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'task-list-test', description: 'Task list test', phases: ['work'] };\n\
         createTaskList('audit', [{ area: 'api' }, { area: 'docs' }]);\n\
         const worker = async (name) => {\n\
           const task = claimTask('audit', { by: name });\n\
           const result = await agent(`handle ${task.value.area}`);\n\
           completeTask('audit', task.id, { prompt: result.prompt }, { by: name });\n\
           return { worker: name, taskId: task.id, area: task.value.area };\n\
         };\n\
         const workers = await phase('work', async () => parallel([worker('worker-a'), worker('worker-b')]));\n\
         export default { workers, tasks: listTasks('audit') };",
    )
    .unwrap();

    let events = WorkflowHost::run_collecting_events(&script, serde_json::json!(null)).unwrap();

    let prompts = events
        .iter()
        .filter_map(|event| match event {
            HostEvent::AgentCall { prompt, phase, .. } if phase.as_deref() == Some("work") => {
                Some(prompt.as_str())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(prompts, vec!["handle api", "handle docs"]);

    assert!(events.iter().any(|event| {
        matches!(
            event,
            HostEvent::WorkflowCompleted { result }
                if result["workers"].as_array().map(|items| items.len()) == Some(2)
                    && result["workers"][0]["taskId"] != result["workers"][1]["taskId"]
                    && result["tasks"].as_array().map(|items| {
                        items.iter().all(|item| {
                            item["status"] == "completed"
                                && item["claimedBy"].as_str().is_some()
                                && item["completedBy"].as_str().is_some()
                                && item["result"]["prompt"].as_str().is_some()
                        })
                    }) == Some(true)
        )
    }));
}

#[test]
fn host_ipc_paths_preserve_messages_and_tasks_across_host_restarts() {
    if !WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let ipc_paths = WorkflowHostIpcPaths {
        mailbox_path: temp.path().join("mailbox.json"),
        task_lists_path: temp.path().join("task-lists.json"),
    };
    let writer_script = temp.path().join("writer.js");
    fs::write(
        &writer_script,
        "export const meta = { name: 'ipc-writer', description: 'IPC writer', phases: [] };\n\
         sendMessage('findings', { severity: 'high' }, { from: 'scanner' });\n\
         createTaskList('audit', [{ area: 'api' }]);\n\
         claimTask('audit', { by: 'worker-a' });\n\
         export default 'written';",
    )
    .unwrap();
    WorkflowHost::run_collecting_events_with_ipc_paths(
        &writer_script,
        serde_json::json!(null),
        &ipc_paths,
    )
    .unwrap();

    let reader_script = temp.path().join("reader.js");
    fs::write(
        &reader_script,
        "export const meta = { name: 'ipc-reader', description: 'IPC reader', phases: [] };\n\
         export default { messages: readMessages('findings'), tasks: listTasks('audit') };",
    )
    .unwrap();
    let events = WorkflowHost::run_collecting_events_with_ipc_paths(
        &reader_script,
        serde_json::json!(null),
        &ipc_paths,
    )
    .unwrap();

    assert!(events.iter().any(|event| {
        matches!(
            event,
            HostEvent::WorkflowCompleted { result }
                if result["messages"][0]["from"] == "scanner"
                    && result["messages"][0]["message"]["severity"] == "high"
                    && result["tasks"][0]["id"] == "workflow-task-1"
                    && result["tasks"][0]["status"] == "running"
                    && result["tasks"][0]["claimedBy"] == "worker-a"
        )
    }));
}

#[test]
fn host_phase_fallback_continue_emits_failed_phase_and_continues() {
    if !WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'fallback-host-test', description: 'Fallback host test', phases: ['scan', 'review'] };\n\
         const scan = await phase('scan', async () => agent('fail scan'), { fallback: 'continue' });\n\
         const review = await phase('review', async () => agent('review anyway'));\n\
         export default { scan, review };",
    )
    .unwrap();

    let events =
        WorkflowHost::run_collecting_events_with_agent(&script, serde_json::json!(null), |call| {
            if call.prompt == "fail scan" {
                Ok(orca_runtime::workflow::host::HostCommand::AgentError {
                    call_id: call.call_id.clone(),
                    error: "scan failed".to_string(),
                })
            } else {
                Ok(orca_runtime::workflow::host::HostCommand::AgentResult {
                    call_id: call.call_id.clone(),
                    result: serde_json::json!({ "prompt": call.prompt }),
                })
            }
        })
        .unwrap();

    assert!(events.iter().any(|event| {
        matches!(
            event,
            HostEvent::PhaseFailed { name, error, .. }
                if name == "scan" && error.contains("scan failed")
        )
    }));
    assert!(events.iter().any(|event| {
        matches!(
            event,
            HostEvent::AgentCall { prompt, phase, .. }
                if prompt == "review anyway" && phase.as_deref() == Some("review")
        )
    }));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, HostEvent::WorkflowCompleted { .. }))
    );
}

#[test]
fn host_phase_fallback_value_returns_value_to_following_phase() {
    if !WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'fallback-value-test', description: 'Fallback value test', phases: ['scan', 'review'] };\n\
         const scan = await phase('scan', async () => agent('fail scan'), { fallback: { value: { recovered: true, source: 'fallback' } } });\n\
         const review = await phase('review', async () => agent(`review recovered=${scan.recovered}`));\n\
         export default { scan, review };",
    )
    .unwrap();

    let events =
        WorkflowHost::run_collecting_events_with_agent(&script, serde_json::json!(null), |call| {
            if call.prompt == "fail scan" {
                Ok(orca_runtime::workflow::host::HostCommand::AgentError {
                    call_id: call.call_id.clone(),
                    error: "scan failed".to_string(),
                })
            } else {
                Ok(orca_runtime::workflow::host::HostCommand::AgentResult {
                    call_id: call.call_id.clone(),
                    result: serde_json::json!({ "prompt": call.prompt }),
                })
            }
        })
        .unwrap();

    assert!(events.iter().any(|event| {
        matches!(
            event,
            HostEvent::PhaseFailed { name, error, fallback }
                if name == "scan" && error.contains("scan failed") && fallback.as_deref() == Some("value")
        )
    }));
    assert!(events.iter().any(|event| {
        matches!(
            event,
            HostEvent::AgentCall { prompt, phase, .. }
                if prompt == "review recovered=true" && phase.as_deref() == Some("review")
        )
    }));
    assert!(events.iter().any(|event| {
        matches!(
            event,
            HostEvent::WorkflowCompleted { result }
                if result["scan"]["recovered"] == true && result["scan"]["source"] == "fallback"
        )
    }));
}

#[test]
fn host_phase_fallback_function_can_run_recovery_agent() {
    if !WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'fallback-function-test', description: 'Fallback function test', phases: ['scan', 'review'] };\n\
         const scan = await phase('scan', async () => agent('fail scan'), { fallback: async ({ error }) => agent(`recover ${error}`) });\n\
         const review = await phase('review', async () => agent(`review recovered=${scan.prompt}`));\n\
         export default { scan, review };",
    )
    .unwrap();

    let events =
        WorkflowHost::run_collecting_events_with_agent(&script, serde_json::json!(null), |call| {
            if call.prompt == "fail scan" {
                Ok(orca_runtime::workflow::host::HostCommand::AgentError {
                    call_id: call.call_id.clone(),
                    error: "scan failed".to_string(),
                })
            } else {
                Ok(orca_runtime::workflow::host::HostCommand::AgentResult {
                    call_id: call.call_id.clone(),
                    result: serde_json::json!({ "prompt": call.prompt }),
                })
            }
        })
        .unwrap();

    assert!(events.iter().any(|event| {
        matches!(
            event,
            HostEvent::PhaseFailed { name, error, fallback }
                if name == "scan" && error.contains("scan failed") && fallback.as_deref() == Some("function")
        )
    }));
    assert!(events.iter().any(|event| {
        matches!(
            event,
            HostEvent::AgentCall { prompt, phase, .. }
                if prompt == "recover scan failed" && phase.as_deref() == Some("scan")
        )
    }));
    assert!(events.iter().any(|event| {
        matches!(
            event,
            HostEvent::AgentCall { prompt, phase, .. }
                if prompt == "review recovered=recover scan failed" && phase.as_deref() == Some("review")
        )
    }));
}

#[test]
fn host_exposes_args_global() {
    if !WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'args-test', description: 'Args test', phases: [] };\nawait agent(args.prompt);\nexport default 'done';",
    )
    .unwrap();

    let events =
        WorkflowHost::run_collecting_events(&script, serde_json::json!({"prompt": "from args"}))
            .unwrap();
    assert!(events.iter().any(
        |event| matches!(event, HostEvent::AgentCall { prompt, .. } if prompt == "from args")
    ));
}

#[test]
fn host_ignores_export_mentions_in_comments_and_strings_when_loading_workflow_module() {
    if !WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "/* export const meta = { fake: true }; */\nconst prompt = 'Prompt mentioning export default before the real workflow body';\nexport const meta = { name: 'rewrite-guard-test', description: 'Syntax-aware export rewrite test', phases: ['scan'] };\nconst result = await phase('scan', async () => agent(prompt, { description: 'scan repo' }));\nexport default result;",
    )
    .unwrap();

    let events = WorkflowHost::run_collecting_events(&script, serde_json::json!(null)).unwrap();

    assert!(
        events
            .iter()
            .any(|event| matches!(event, HostEvent::PhaseStarted { name } if name == "scan"))
    );
    assert!(events.iter().any(|event| {
        matches!(
            event,
            HostEvent::AgentCall { prompt, .. }
                if prompt == "Prompt mentioning export default before the real workflow body"
        )
    }));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, HostEvent::WorkflowCompleted { .. }))
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, HostEvent::WorkflowFailed { .. }))
    );
}

#[test]
fn host_executes_top_level_phase_task_definitions() {
    if !WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'dsl-test', description: 'DSL test' };\nexport const phases = [{ name: 'scan', tasks: [{ type: 'agent', description: 'scan repo', prompt: 'inspect repo', model: 'deepseek-flash' }] }, { name: 'review', tasks: [{ type: 'agent', description: 'review scan', prompt: 'review previous output' }] }];",
    )
    .unwrap();

    let events = WorkflowHost::run_collecting_events(&script, serde_json::json!(null)).unwrap();

    assert!(
        events
            .iter()
            .any(|event| matches!(event, HostEvent::PhaseStarted { name } if name == "scan"))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, HostEvent::PhaseStarted { name } if name == "review"))
    );
    assert!(events.iter().any(|event| {
        matches!(
            event,
            HostEvent::AgentCall { prompt, opts, .. }
                if prompt == "inspect repo" && opts["description"] == "scan repo"
        )
    }));
    assert!(events.iter().any(|event| {
        matches!(
            event,
            HostEvent::AgentCall { prompt, .. }
                if prompt.contains("[Previous phase outputs]") && prompt.contains("review previous output")
        )
    }));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, HostEvent::WorkflowCompleted { .. }))
    );
}

#[test]
fn host_executes_meta_phase_task_definitions() {
    if !WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'dsl-test', description: 'DSL test', phases: [{ name: 'scan', tasks: [{ description: 'scan repo', prompt: 'inspect repo' }] }] };",
    )
    .unwrap();

    let events = WorkflowHost::run_collecting_events(&script, serde_json::json!(null)).unwrap();

    assert!(
        events
            .iter()
            .any(|event| matches!(event, HostEvent::PhaseStarted { name } if name == "scan"))
    );
    assert!(events.iter().any(|event| {
        matches!(
            event,
            HostEvent::AgentCall { prompt, opts, .. }
                if prompt == "inspect repo" && opts["description"] == "scan repo"
        )
    }));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, HostEvent::WorkflowCompleted { .. }))
    );
}

#[test]
fn host_allows_blocked_words_in_comments_and_prompt_strings() {
    if !WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'string-comment-test', description: 'String and comment handling test', phases: [] };\n// Mentioning child_process here should stay harmless.\nawait agent('inspect process usage and globalThis references');\nexport default 'done';",
    )
    .unwrap();

    let events = WorkflowHost::run_collecting_events(&script, serde_json::json!(null)).unwrap();

    assert!(events.iter().any(|event| {
        matches!(
            event,
            HostEvent::AgentCall { prompt, .. }
                if prompt == "inspect process usage and globalThis references"
        )
    }));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, HostEvent::WorkflowCompleted { .. }))
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, HostEvent::WorkflowFailed { .. }))
    );
}

#[test]
fn host_blocks_constructor_process_escape_attempts() {
    if !WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'constructor-escape-test', description: 'Constructor process escape test', phases: [] };\nconst escaped = globalThis.constructor.constructor('return process')();\nawait agent(`escaped ${escaped.version}`);\nexport default null;",
    )
    .unwrap();

    let events = WorkflowHost::run_collecting_events(&script, serde_json::json!(null)).unwrap();

    assert!(
        events
            .iter()
            .any(|event| matches!(event, HostEvent::WorkflowFailed { .. }))
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, HostEvent::AgentCall { prompt, .. } if prompt.starts_with("escaped ")))
    );
    assert!(!events.iter().any(|event| {
        matches!(event, HostEvent::AgentCall { prompt, .. } if prompt == "escaped process")
    }));
}

#[test]
fn host_blocks_bracket_constructor_process_escape_attempts() {
    if !WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'bracket-constructor-escape-test', description: 'Bracket constructor process escape test', phases: [] };\nconst escaped = ({})['constructor']['constructor']('return process')();\nawait agent(`escaped ${escaped.version}`);\nexport default null;",
    )
    .unwrap();

    let events = WorkflowHost::run_collecting_events(&script, serde_json::json!(null)).unwrap();

    assert!(events.iter().any(|event| matches!(
        event,
        HostEvent::WorkflowFailed { error }
            if error.contains("prohibited computed property: constructor")
    )));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, HostEvent::AgentCall { prompt, .. } if prompt.starts_with("escaped ")))
    );
}

#[test]
fn host_blocks_constructor_builtin_module_escape_attempts() {
    if !WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'builtin-escape-test', description: 'Built-in module escape test', phases: [] };\nconst processRef = globalThis.constructor.constructor('return process')();\nconst fsRef = processRef.getBuiltinModule('node:fs');\nawait agent(`escaped fs ${typeof fsRef.readFileSync}`);\nexport default null;",
    )
    .unwrap();

    let events = WorkflowHost::run_collecting_events(&script, serde_json::json!(null)).unwrap();

    assert!(
        events
            .iter()
            .any(|event| matches!(event, HostEvent::WorkflowFailed { .. }))
    );
    assert!(!events.iter().any(|event| {
        matches!(event, HostEvent::AgentCall { prompt, .. } if prompt.starts_with("escaped fs "))
    }));
}

#[test]
fn host_blocks_bracket_builtin_module_escape_attempts() {
    if !WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'bracket-builtin-escape-test', description: 'Bracket built-in module escape test', phases: [] };\nconst processRef = { ['getBuiltinModule']: () => ({ readFileSync() {} }) };\nconst fsRef = processRef['getBuiltinModule']('node:fs');\nawait agent(`escaped fs ${typeof fsRef.readFileSync}`);\nexport default null;",
    )
    .unwrap();

    let events = WorkflowHost::run_collecting_events(&script, serde_json::json!(null)).unwrap();

    assert!(events.iter().any(|event| matches!(
        event,
        HostEvent::WorkflowFailed { error }
            if error.contains("prohibited computed property: getBuiltinModule")
    )));
    assert!(!events.iter().any(|event| {
        matches!(event, HostEvent::AgentCall { prompt, .. } if prompt.starts_with("escaped fs "))
    }));
}

#[test]
fn host_returns_workflow_failed_event_for_script_exceptions() {
    if !WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'failure-test', description: 'Failure propagation test', phases: [] };\nthrow new Error('boom from script');",
    )
    .unwrap();

    let events = WorkflowHost::run_collecting_events(&script, serde_json::json!(null)).unwrap();

    assert!(events.iter().any(|event| {
        matches!(
            event,
            HostEvent::WorkflowFailed { error } if error.contains("boom from script")
        )
    }));
}

#[test]
fn host_rejects_oversized_event_frames() {
    if !WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'oversized-frame', description: 'Oversized frame', phases: [] };\nexport default 'x'.repeat(1024 * 1024 + 1);",
    )
    .unwrap();

    let result = WorkflowHost::run_collecting_events(&script, serde_json::json!(null));
    let Err(error) = result else {
        panic!("oversized workflow host frame should fail closed");
    };

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("frame"));
}

#[test]
fn host_rejects_event_floods() {
    if !WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'event-flood', description: 'Event flood', phases: [] };\nfor (let index = 0; index < 8200; index += 1) { phase(`phase-${index}`); }\nexport default 'done';",
    )
    .unwrap();

    let result = WorkflowHost::run_collecting_events(&script, serde_json::json!(null));
    let Err(error) = result else {
        panic!("workflow host event flood should fail closed");
    };

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("event"));
}

#[test]
fn host_bounds_default_agent_call_worker_concurrency() {
    if !WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'bounded-workers', description: 'Bounded workers', phases: [] };\nconst calls = Array.from({ length: 32 }, (_, index) => agent(`agent-${index}`));\nexport default await parallel(calls);",
    )
    .unwrap();
    let active = Arc::new(AtomicUsize::new(0));
    let max_active = Arc::new(AtomicUsize::new(0));

    WorkflowHost::run_collecting_events_with_agent(&script, serde_json::json!(null), {
        let active = Arc::clone(&active);
        let max_active = Arc::clone(&max_active);
        move |call| {
            let now = active.fetch_add(1, Ordering::SeqCst) + 1;
            max_active.fetch_max(now, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(25));
            active.fetch_sub(1, Ordering::SeqCst);
            Ok(orca_runtime::workflow::host::HostCommand::AgentResult {
                call_id: call.call_id,
                result: serde_json::json!(null),
            })
        }
    })
    .unwrap();

    assert!(
        max_active.load(Ordering::SeqCst) <= 16,
        "default host worker count must bound callback concurrency"
    );
}

#[test]
fn host_reports_workflow_failed_when_stdin_closes_before_agent_result() {
    if !WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'stdin-eof-test', description: 'stdin eof', phases: [] };\nawait agent('inspect repo');\nexport default 'done';",
    )
    .unwrap();

    let host = temp.path().join("host.mjs");
    fs::write(&host, WORKFLOW_HOST_SCRIPT).unwrap();

    let mut child = Command::new(WorkflowHost::node_executable())
        .arg(&host)
        .arg(&script)
        .arg("null")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut first_line = String::new();
    reader.read_line(&mut first_line).unwrap();
    assert!(first_line.contains("\"type\":\"agent_call\""));

    let stdin = child.stdin.take().unwrap();
    drop(stdin);

    let mut remaining = Vec::new();
    for line in reader.lines() {
        remaining.push(line.unwrap());
    }

    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "expected host to exit with workflow failure, status={:?}, stderr={stderr}",
        output.status.code()
    );
    assert!(
        remaining
            .iter()
            .any(|line| line.contains("\"type\":\"workflow_failed\""))
    );
}

#[test]
fn host_reports_workflow_failed_for_partial_trailing_json_on_stdin_eof() {
    if !WorkflowHost::node_available() {
        return;
    }

    let temp = tempdir().unwrap();
    let script = temp.path().join("workflow.js");
    fs::write(
        &script,
        "export const meta = { name: 'stdin-partial-json-test', description: 'stdin partial json', phases: [] };\nawait agent('inspect repo');\nexport default 'done';",
    )
    .unwrap();

    let host = temp.path().join("host.mjs");
    fs::write(&host, WORKFLOW_HOST_SCRIPT).unwrap();

    let mut child = Command::new(WorkflowHost::node_executable())
        .arg(&host)
        .arg(&script)
        .arg("null")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut first_line = String::new();
    reader.read_line(&mut first_line).unwrap();
    assert!(first_line.contains("\"type\":\"agent_call\""));

    let mut stdin = child.stdin.take().unwrap();
    use std::io::Write;
    stdin.write_all(br#"{"type":"agent_result""#).unwrap();
    drop(stdin);

    let mut remaining = Vec::new();
    for line in reader.lines() {
        remaining.push(line.unwrap());
    }

    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "expected host to exit with workflow failure, status={:?}, stderr={stderr}",
        output.status.code()
    );
    assert!(remaining.iter().any(|line| {
        line.contains("\"type\":\"workflow_failed\"") && line.contains("partial JSON")
    }));
}
