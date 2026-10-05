use super::*;
use crate::client::should_reconnect_after_mcp_error;
use crate::transport::{
    McpElicitationHandler, McpElicitationMode, McpElicitationRequest, McpElicitationResponse,
    McpTransport,
};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const STDIO_TEST_STARTUP_TIMEOUT_MS: u64 = 15_000;

/// The registry of `configs` once each server has connected or failed.
fn connected_registry(
    configs: &[McpServerConfig],
    credentials_path: Option<PathBuf>,
) -> McpRegistry {
    let registry = initialize_registry(configs, credentials_path);
    assert!(registry.wait_for_startup(&|| false));
    registry
}

#[cfg(unix)]
fn stdio_fixture_config(name: &str, server: &std::path::Path) -> McpServerConfig {
    McpServerConfig {
        name: name.to_string(),
        transport: orca_core::mcp_types::McpTransportKind::Stdio,
        command: Some("/bin/sh".to_string()),
        args: vec![server.to_string_lossy().into_owned()],
        url: None,
        env: Default::default(),
        headers: Default::default(),
        disabled: false,
        capabilities: Default::default(),
        startup_timeout_ms: Some(STDIO_TEST_STARTUP_TIMEOUT_MS),
        tool_timeout_ms: Some(1000),
        ..Default::default()
    }
}

#[cfg(unix)]
#[test]
fn stdio_fixture_config_runs_shell_script_through_sh() {
    let config = stdio_fixture_config("templates", std::path::Path::new("/tmp/mcp.sh"));

    assert_eq!(config.command.as_deref(), Some("/bin/sh"));
    assert_eq!(config.args, vec!["/tmp/mcp.sh"]);
}

#[cfg(unix)]
#[test]
fn a_tool_marked_read_only_by_its_server_is_read_only() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let server = temp_dir.path().join("annotated_tools_mcp_server.sh");
    std::fs::write(
            &server,
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"annotated","version":"1"}}}\n'
      ;;
    *'"method":"notifications/initialized"'*)
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"read_file","description":"reads a file","inputSchema":{"type":"object","properties":{},"required":[]},"annotations":{"readOnlyHint":true}},{"name":"write_file","description":"writes a file","inputSchema":{"type":"object","properties":{},"required":[]}}]}}\n'
      ;;
  esac
done
"#,
        )
        .expect("write MCP fixture");
    let config = stdio_fixture_config("annotated", &server);

    let registry = connected_registry(&[config], None);

    assert!(registry.errors().is_empty(), "{:?}", registry.errors());
    let tools = registry.tools();
    let read_tool = tools
        .iter()
        .find(|tool| tool.name == "read_file")
        .expect("read_file tool");
    let write_tool = tools
        .iter()
        .find(|tool| tool.name == "write_file")
        .expect("write_file tool");
    assert!(read_tool.read_only);
    assert!(!write_tool.read_only);
}

#[test]
fn filtered_tools_are_not_registered() {
    struct FixedToolsTransport;

    impl McpTransport for FixedToolsTransport {
        fn initialize(&self) -> Result<Value, String> {
            Ok(serde_json::json!({"capabilities": {}}))
        }

        fn list_tools(&self, _cursor: Option<&str>) -> Result<Value, String> {
            Ok(serde_json::json!({"tools": [
                {"name": "a", "inputSchema": {"type": "object"}},
                {"name": "b", "inputSchema": {"type": "object"}},
                {"name": "c", "inputSchema": {"type": "object"}}
            ]}))
        }

        fn call_tool(&self, _name: &str, _arguments: Value) -> Result<Value, String> {
            Err("fixed tools transport does not support tool calls".to_string())
        }

        fn list_resources(&self, _cursor: Option<&str>) -> Result<Value, String> {
            Ok(serde_json::json!({"resources": []}))
        }

        fn list_resource_templates(&self, _cursor: Option<&str>) -> Result<Value, String> {
            Ok(serde_json::json!({"resourceTemplates": []}))
        }

        fn read_resource(&self, _uri: &str) -> Result<Value, String> {
            Err("fixed tools transport does not support resource reads".to_string())
        }
    }

    let config = McpServerConfig {
        name: "filtered".to_string(),
        disabled_tools: Some(vec!["b".to_string()]),
        ..Default::default()
    };

    let ConnectedServer { client, tools, .. } = connect_server_with_transport(
        &config,
        "filtered",
        None,
        Arc::new(FixedToolsTransport),
        Arc::default(),
    )
    .expect("connect with fixed tools transport");
    let lookup = tools
        .iter()
        .map(|tool| {
            (
                tool.schema_name.clone(),
                McpToolRef {
                    server: tool.server.clone(),
                    tool: tool.name.clone(),
                    schema_name: tool.schema_name.clone(),
                },
            )
        })
        .collect();
    let registry = McpRegistry::from_inner(McpRegistryInner {
        clients: HashMap::from([("filtered".to_string(), Arc::new(client))]),
        tools,
        lookup,
        ..Default::default()
    });

    let names: Vec<String> = registry.tools().into_iter().map(|tool| tool.name).collect();
    assert_eq!(names, vec!["a", "c"]);
}

#[test]
fn normalizes_non_object_schema() {
    let schema = normalize_schema(Value::Null);
    assert_eq!(schema["type"], "object");
}

#[test]
fn tools_preserve_insertion_order() {
    // The DeepSeek tool schema is byte-pinned for prefix caching, so the
    // registry must expose tools in a stable order. `tools()` is backed by a
    // Vec (insertion order), never a HashMap; lock that here so a future
    // refactor to a map-backed store fails loudly.
    let make = |schema_name: &str| McpTool {
        server: "srv".to_string(),
        name: schema_name.to_string(),
        schema_name: schema_name.to_string(),
        description: None,
        input_schema: serde_json::json!({"type": "object"}),
        read_only: false,
    };
    let order = vec!["mcp__srv__zzz", "mcp__srv__aaa", "mcp__srv__mmm"];
    let registry =
        McpRegistry::from_tools_for_test(order.iter().map(|n| make(n)).collect::<Vec<_>>());
    let got: Vec<String> = registry
        .tools()
        .into_iter()
        .map(|tool| tool.schema_name)
        .collect();
    assert_eq!(got, order);
}

#[test]
fn call_tool_or_cancel_waits_for_transport_cleanup() {
    struct CleanupAwareTransport {
        active: Arc<AtomicBool>,
        release: Arc<AtomicBool>,
    }

    impl McpTransport for CleanupAwareTransport {
        fn initialize(&self) -> Result<Value, String> {
            Ok(serde_json::json!({"capabilities": {"resources": {}}}))
        }

        fn list_tools(&self, _cursor: Option<&str>) -> Result<Value, String> {
            Ok(serde_json::json!({"tools": []}))
        }

        fn call_tool(&self, _name: &str, _arguments: Value) -> Result<Value, String> {
            self.active.store(true, Ordering::SeqCst);
            while !self.release.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
            self.active.store(false, Ordering::SeqCst);
            Ok(serde_json::json!({
                "content": [{"type": "text", "text": "too late"}],
                "isError": false
            }))
        }

        fn call_tool_with_elicitation_handler_or_cancel(
            &self,
            _name: &str,
            _arguments: Value,
            _handler: Option<&dyn McpElicitationHandler>,
            should_cancel: &dyn Fn() -> bool,
        ) -> Result<Value, String> {
            self.active.store(true, Ordering::SeqCst);
            while !should_cancel() {
                std::thread::sleep(Duration::from_millis(5));
            }
            self.active.store(false, Ordering::SeqCst);
            Err("MCP tool call cancelled".to_string())
        }

        fn list_resources(&self, _cursor: Option<&str>) -> Result<Value, String> {
            Ok(serde_json::json!({"resources": []}))
        }

        fn list_resource_templates(&self, _cursor: Option<&str>) -> Result<Value, String> {
            Ok(serde_json::json!({"resourceTemplates": []}))
        }

        fn read_resource(&self, _uri: &str) -> Result<Value, String> {
            Ok(serde_json::json!({"contents": []}))
        }
    }

    let tool = McpTool {
        server: "slow".to_string(),
        name: "wait".to_string(),
        schema_name: "mcp__slow__wait".to_string(),
        description: None,
        input_schema: serde_json::json!({"type": "object"}),
        read_only: false,
    };
    let active = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let registry = McpRegistry::from_inner(McpRegistryInner {
        clients: HashMap::from([(
            "slow".to_string(),
            Arc::new(McpClient {
                config: McpServerConfig {
                    name: "slow".to_string(),
                    ..Default::default()
                },
                server_name: "slow".to_string(),
                credentials_path: None,
                capabilities: McpServerCapabilities::resource_capable_for_test(),
                reconnect_failure: Mutex::default(),
                stop: Arc::default(),
                transport: Mutex::new(Arc::new(CleanupAwareTransport {
                    active: Arc::clone(&active),
                    release: Arc::clone(&release),
                })),
            }),
        )]),
        tools: vec![tool.clone()],
        lookup: HashMap::from([(
            tool.schema_name.clone(),
            McpToolRef {
                server: tool.server,
                tool: tool.name,
                schema_name: tool.schema_name,
            },
        )]),
        ..Default::default()
    });
    let tool_ref = registry
        .resolve_tool("mcp__slow__wait")
        .expect("tool ref for slow MCP tool");
    let started = Instant::now();

    let result =
        registry.call_tool_or_cancel(&tool_ref, Value::Object(Default::default()), &|| {
            active.load(Ordering::SeqCst) && started.elapsed() >= Duration::from_millis(50)
        });

    let worker_active_at_return = active.load(Ordering::SeqCst);
    release.store(true, Ordering::SeqCst);
    let cleanup_deadline = Instant::now() + Duration::from_secs(1);
    while active.load(Ordering::SeqCst) && Instant::now() < cleanup_deadline {
        std::thread::sleep(Duration::from_millis(5));
    }

    assert!(started.elapsed() < Duration::from_millis(750));
    assert_eq!(result.unwrap_err(), "MCP tool call cancelled");
    assert!(
        !worker_active_at_return,
        "call_tool_or_cancel returned before its transport worker finished"
    );
}

/// A tiny, valid 1x1 PNG (base64-encoded). Mirrors the constant
/// `orca_core::tool_images`'s own tests define, per that module's note
/// that each caller should define its own rather than share one.
const BASE64_1X1_PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

/// Builds a registry with a single MCP server ("screen") whose only tool
/// ("capture") always returns `content` from `call_tool`.
fn registry_with_call_tool_result(content: Value) -> (McpRegistry, McpToolRef) {
    struct StaticCallToolTransport {
        content: Value,
    }

    impl McpTransport for StaticCallToolTransport {
        fn initialize(&self) -> Result<Value, String> {
            Ok(serde_json::json!({"capabilities": {}}))
        }

        fn list_tools(&self, _cursor: Option<&str>) -> Result<Value, String> {
            Ok(serde_json::json!({"tools": []}))
        }

        fn call_tool(&self, _name: &str, _arguments: Value) -> Result<Value, String> {
            Ok(self.content.clone())
        }

        fn list_resources(&self, _cursor: Option<&str>) -> Result<Value, String> {
            Ok(serde_json::json!({"resources": []}))
        }

        fn list_resource_templates(&self, _cursor: Option<&str>) -> Result<Value, String> {
            Ok(serde_json::json!({"resourceTemplates": []}))
        }

        fn read_resource(&self, _uri: &str) -> Result<Value, String> {
            Ok(serde_json::json!({"contents": []}))
        }
    }

    let tool = McpTool {
        server: "screen".to_string(),
        name: "capture".to_string(),
        schema_name: "mcp__screen__capture".to_string(),
        description: None,
        input_schema: serde_json::json!({"type": "object"}),
        read_only: false,
    };
    let registry = McpRegistry::from_inner(McpRegistryInner {
        clients: HashMap::from([(
            "screen".to_string(),
            Arc::new(McpClient {
                config: McpServerConfig {
                    name: "screen".to_string(),
                    ..Default::default()
                },
                server_name: "screen".to_string(),
                credentials_path: None,
                capabilities: McpServerCapabilities::default(),
                reconnect_failure: Mutex::default(),
                stop: Arc::default(),
                transport: Mutex::new(Arc::new(StaticCallToolTransport { content })),
            }),
        )]),
        tools: vec![tool.clone()],
        lookup: HashMap::from([(
            tool.schema_name.clone(),
            McpToolRef {
                server: tool.server,
                tool: tool.name,
                schema_name: tool.schema_name,
            },
        )]),
        ..Default::default()
    });
    let tool_ref = registry
        .resolve_tool("mcp__screen__capture")
        .expect("tool ref for screen capture");
    (registry, tool_ref)
}

#[test]
fn an_image_from_an_mcp_tool_reaches_the_tool_output() {
    let (registry, tool_ref) = registry_with_call_tool_result(serde_json::json!({
        "content": [
            {"type": "text", "text": "screenshot taken"},
            {"type": "image", "data": BASE64_1X1_PNG, "mimeType": "image/png"}
        ],
        "isError": false
    }));

    let result = registry
        .call_tool(&tool_ref, serde_json::json!({}))
        .expect("tool result");

    assert_eq!(result.output, "screenshot taken\n[1 image attached]");
    assert_eq!(result.images.len(), 1);
    assert!(matches!(
        &result.images[0].source,
        orca_core::conversation::ImageSource::Base64 { media_type, .. }
            if media_type == "image/png"
    ));
}

#[test]
fn an_unsupported_mcp_image_leaves_a_note() {
    let (registry, tool_ref) = registry_with_call_tool_result(serde_json::json!({
        "content": [
            {"type": "text", "text": "screenshot taken"},
            {"type": "image", "data": "PHN2Zz4=", "mimeType": "image/svg+xml"}
        ],
        "isError": false
    }));

    let result = registry
        .call_tool(&tool_ref, serde_json::json!({}))
        .expect("tool result");

    assert_eq!(
        result.output,
        "screenshot taken\n[image omitted: unsupported type image/svg+xml]"
    );
    assert!(result.images.is_empty());
}

#[test]
fn an_mcp_image_without_a_media_type_leaves_a_note_instead_of_failing_the_call() {
    let (registry, tool_ref) = registry_with_call_tool_result(serde_json::json!({
        "content": [
            {"type": "text", "text": "screenshot taken"},
            {"type": "image", "data": BASE64_1X1_PNG}
        ],
        "isError": false
    }));

    let result = registry
        .call_tool(&tool_ref, serde_json::json!({}))
        .expect("a malformed image block must not fail the call");

    assert_eq!(
        result.output,
        "screenshot taken\n[image omitted: missing media type]"
    );
    assert!(result.images.is_empty());
}

#[test]
fn a_tool_whose_only_text_block_is_empty_reports_no_text_content() {
    let (registry, tool_ref) = registry_with_call_tool_result(serde_json::json!({
        "content": [{"type": "text", "text": ""}],
        "isError": false
    }));

    let result = registry
        .call_tool(&tool_ref, serde_json::json!({}))
        .expect("tool result");

    assert_eq!(result.output, "(MCP tool returned no text content)");
}

#[test]
fn two_accepted_mcp_images_share_one_plural_marker() {
    let (registry, tool_ref) = registry_with_call_tool_result(serde_json::json!({
        "content": [
            {"type": "text", "text": "screenshots taken"},
            {"type": "image", "data": BASE64_1X1_PNG, "mimeType": "image/png"},
            {"type": "image", "data": BASE64_1X1_PNG, "mimeType": "image/png"}
        ],
        "isError": false
    }));

    let result = registry
        .call_tool(&tool_ref, serde_json::json!({}))
        .expect("tool result");

    assert_eq!(result.output, "screenshots taken\n[2 images attached]");
    assert_eq!(result.images.len(), 2);
}

#[test]
fn rejected_only_mcp_images_leave_only_their_notes() {
    let (registry, tool_ref) = registry_with_call_tool_result(serde_json::json!({
        "content": [
            {"type": "image", "data": "PHN2Zz4=", "mimeType": "image/svg+xml"},
            {"type": "image", "data": "not base64!", "mimeType": "image/png"}
        ],
        "isError": false
    }));

    let result = registry
        .call_tool(&tool_ref, serde_json::json!({}))
        .expect("tool result");

    assert_eq!(
        result.output,
        "[image omitted: unsupported type image/svg+xml]\n[image omitted: invalid base64 data]"
    );
    assert!(result.images.is_empty());
}

#[test]
fn mixed_mcp_content_lists_text_then_notes_then_the_image_marker() {
    let (registry, tool_ref) = registry_with_call_tool_result(serde_json::json!({
        "content": [
            {"type": "image", "data": BASE64_1X1_PNG, "mimeType": "image/png"},
            {"type": "text", "text": "screenshot taken"},
            {"type": "image", "data": "PHN2Zz4=", "mimeType": "image/svg+xml"}
        ],
        "isError": false
    }));

    let result = registry
        .call_tool(&tool_ref, serde_json::json!({}))
        .expect("tool result");

    assert_eq!(
        result.output,
        "screenshot taken\n[image omitted: unsupported type image/svg+xml]\n[1 image attached]"
    );
    assert_eq!(result.images.len(), 1);
}

#[cfg(unix)]
#[test]
fn cancelled_stdio_tool_call_reconnects_before_returning() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let server = temp_dir.path().join("reconnecting_mcp_server.sh");
    let generation_file = temp_dir.path().join("generation");
    let pid_prefix = temp_dir.path().join("pid");
    let started_file = temp_dir.path().join("started");
    std::fs::write(
            &server,
            r#"#!/bin/sh
generation=0
if [ -f "$GENERATION_FILE" ]; then
  IFS= read -r generation < "$GENERATION_FILE"
fi
generation=$((generation + 1))
printf '%s' "$generation" > "$GENERATION_FILE"
printf '%s' "$$" > "$PID_PREFIX.$generation"
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"reconnect","version":"1"}}}\n'
      ;;
    *'"method":"notifications/initialized"'*)
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"wait","description":"waits","inputSchema":{"type":"object","properties":{},"required":[]}}]}}\n'
      ;;
    *'"method":"tools/call"'*)
      printf started > "$STARTED_FILE"
      if [ "$generation" -eq 1 ]; then
        IFS= read -r ignored
      else
        printf '{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"generation 2"}],"isError":false}}\n'
      fi
      ;;
  esac
done
"#,
        )
        .expect("write MCP fixture");
    let mut config = stdio_fixture_config("reconnect", &server);
    config.env = HashMap::from([
        (
            "GENERATION_FILE".to_string(),
            generation_file.to_string_lossy().into_owned(),
        ),
        (
            "PID_PREFIX".to_string(),
            pid_prefix.to_string_lossy().into_owned(),
        ),
        (
            "STARTED_FILE".to_string(),
            started_file.to_string_lossy().into_owned(),
        ),
    ]);
    config.tool_timeout_ms = Some(500);
    let registry = connected_registry(&[config], None);
    assert!(registry.errors().is_empty(), "{:?}", registry.errors());
    let tool_ref = registry
        .resolve_tool("mcp__reconnect__wait")
        .expect("reconnect tool ref");

    let result =
        registry.call_tool_or_cancel(&tool_ref, Value::Object(Default::default()), &|| {
            started_file.exists()
        });

    let generation_at_return =
        std::fs::read_to_string(&generation_file).expect("generation at cancellation return");
    let first_pid =
        std::fs::read_to_string(pid_prefix.with_extension("1")).expect("first server pid");
    let first_pid_alive_at_return = process_is_alive(first_pid.trim());

    let cleanup_deadline = Instant::now() + Duration::from_secs(2);
    while (std::fs::read_to_string(&generation_file).ok().as_deref() != Some("2")
        || process_is_alive(first_pid.trim()))
        && Instant::now() < cleanup_deadline
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    let second = registry
        .call_tool(&tool_ref, Value::Object(Default::default()))
        .expect("tool call after reconnect");

    assert_eq!(result.unwrap_err(), "MCP tool call cancelled");
    assert_eq!(
        generation_at_return, "2",
        "reconnect must finish before return"
    );
    assert!(
        !first_pid_alive_at_return,
        "cancelled stdio server must be reaped before return"
    );
    assert_eq!(second.output, "generation 2");
}

#[cfg(unix)]
#[test]
fn cancelled_stdio_resource_listing_reconnects_before_returning() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let server = temp_dir.path().join("reconnecting_resource_server.sh");
    let generation_file = temp_dir.path().join("resource-generation");
    let pid_prefix = temp_dir.path().join("resource-pid");
    let started_file = temp_dir.path().join("resource-started");
    std::fs::write(
            &server,
            r#"#!/bin/sh
generation=0
if [ -f "$GENERATION_FILE" ]; then
  IFS= read -r generation < "$GENERATION_FILE"
fi
generation=$((generation + 1))
printf '%s' "$generation" > "$GENERATION_FILE"
printf '%s' "$$" > "$PID_PREFIX.$generation"
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{"resources":{}},"serverInfo":{"name":"resource-reconnect","version":"1"}}}\n'
      ;;
    *'"method":"notifications/initialized"'*)
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[]}}\n'
      ;;
    *'"method":"resources/list"'*)
      printf started > "$STARTED_FILE"
      if [ "$generation" -eq 1 ]; then
        IFS= read -r ignored
      else
        printf '{"jsonrpc":"2.0","id":3,"result":{"resources":[{"uri":"memo://generation-2","name":"generation 2"}]}}\n'
      fi
      ;;
  esac
done
"#,
        )
        .expect("write MCP fixture");
    let mut config = stdio_fixture_config("resource_reconnect", &server);
    config.env = HashMap::from([
        (
            "GENERATION_FILE".to_string(),
            generation_file.to_string_lossy().into_owned(),
        ),
        (
            "PID_PREFIX".to_string(),
            pid_prefix.to_string_lossy().into_owned(),
        ),
        (
            "STARTED_FILE".to_string(),
            started_file.to_string_lossy().into_owned(),
        ),
    ]);
    config.tool_timeout_ms = Some(5_000);
    let registry = connected_registry(&[config], None);
    assert!(registry.errors().is_empty(), "{:?}", registry.errors());

    let result = registry.list_resources_with_errors_or_cancel(None, &|| started_file.exists());

    let generation_at_return =
        std::fs::read_to_string(&generation_file).expect("generation at cancellation return");
    let first_pid =
        std::fs::read_to_string(pid_prefix.with_extension("1")).expect("first server pid");
    let first_pid_alive_at_return = process_is_alive(first_pid.trim());
    let second = registry
        .list_resources(Some("resource_reconnect"))
        .expect("resource listing after reconnect");

    assert_eq!(result.unwrap_err(), McpRequestError::Cancelled);
    assert_eq!(
        generation_at_return, "2",
        "resource reconnect must finish before return"
    );
    assert!(
        !first_pid_alive_at_return,
        "cancelled resource server must be reaped before return"
    );
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].uri, "memo://generation-2");
}

#[cfg(unix)]
#[test]
fn cancelled_stdio_tool_call_bounds_failed_reconnect_handshake() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let server = temp_dir.path().join("stalled_reconnect_mcp_server.sh");
    let generation_file = temp_dir.path().join("generation");
    let pid_prefix = temp_dir.path().join("pid");
    let started_file = temp_dir.path().join("started");
    std::fs::write(
            &server,
            r#"#!/bin/sh
generation=0
if [ -f "$GENERATION_FILE" ]; then
  IFS= read -r generation < "$GENERATION_FILE"
fi
generation=$((generation + 1))
printf '%s' "$generation" > "$GENERATION_FILE"
printf '%s' "$$" > "$PID_PREFIX.$generation"
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      if [ "$generation" -eq 1 ]; then
        printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"reconnect","version":"1"}}}\n'
      else
        sleep 10
      fi
      ;;
    *'"method":"notifications/initialized"'*)
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"wait","description":"waits","inputSchema":{"type":"object","properties":{},"required":[]}}]}}\n'
      ;;
    *'"method":"tools/call"'*)
      printf started > "$STARTED_FILE"
      IFS= read -r ignored
      ;;
  esac
done
"#,
        )
        .expect("write MCP fixture");
    let mut config = stdio_fixture_config("stalled_reconnect", &server);
    config.env = HashMap::from([
        (
            "GENERATION_FILE".to_string(),
            generation_file.to_string_lossy().into_owned(),
        ),
        (
            "PID_PREFIX".to_string(),
            pid_prefix.to_string_lossy().into_owned(),
        ),
        (
            "STARTED_FILE".to_string(),
            started_file.to_string_lossy().into_owned(),
        ),
    ]);
    config.startup_timeout_ms = Some(3_000);
    let registry = connected_registry(&[config], None);
    assert!(registry.errors().is_empty(), "{:?}", registry.errors());
    let tool_ref = registry
        .resolve_tool("mcp__stalled_reconnect__wait")
        .expect("stalled reconnect tool ref");
    let started = Instant::now();

    let result =
        registry.call_tool_or_cancel(&tool_ref, Value::Object(Default::default()), &|| {
            started_file.exists()
        });

    let elapsed = started.elapsed();
    let generation_at_return =
        std::fs::read_to_string(&generation_file).expect("generation at cancellation return");
    let second_pid =
        std::fs::read_to_string(pid_prefix.with_extension("2")).expect("second server pid");

    assert_eq!(result.unwrap_err(), "MCP tool call cancelled");
    assert_eq!(generation_at_return, "2", "reconnect must be attempted");
    assert!(
        elapsed < Duration::from_secs(2),
        "failed reconnect delayed cancellation for {elapsed:?}"
    );
    assert!(
        !process_is_alive(second_pid.trim()),
        "failed reconnect server must be reaped before return"
    );
}

#[cfg(unix)]
#[test]
fn a_cancelled_call_does_not_fail_a_server_slow_to_restart() {
    // Every restart takes 1 s to answer `initialize`: longer than the
    // capped reconnect after a cancelled call, shorter than the server's
    // own startup timeout.
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let server = temp_dir.path().join("slow_restart_mcp_server.sh");
    let generation_file = temp_dir.path().join("generation");
    let started_file = temp_dir.path().join("started");
    std::fs::write(
            &server,
            r#"#!/bin/sh
generation=0
if [ -f "$GENERATION_FILE" ]; then
  IFS= read -r generation < "$GENERATION_FILE"
fi
generation=$((generation + 1))
printf '%s' "$generation" > "$GENERATION_FILE"
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      if [ "$generation" -gt 1 ]; then
        sleep 1
      fi
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"slow","version":"1"}}}\n'
      ;;
    *'"method":"notifications/initialized"'*)
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"wait","description":"waits","inputSchema":{"type":"object","properties":{},"required":[]}}]}}\n'
      ;;
    *'"method":"tools/call"'*)
      if [ "$generation" -eq 1 ]; then
        printf started > "$STARTED_FILE"
        IFS= read -r ignored
      else
        id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
        printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"answered by %s"}],"isError":false}}\n' "$id" "$generation"
      fi
      ;;
  esac
done
"#,
        )
        .expect("write MCP fixture");
    let mut config = stdio_fixture_config("slow", &server);
    config.env = HashMap::from([
        (
            "GENERATION_FILE".to_string(),
            generation_file.to_string_lossy().into_owned(),
        ),
        (
            "STARTED_FILE".to_string(),
            started_file.to_string_lossy().into_owned(),
        ),
    ]);
    config.startup_timeout_ms = Some(5_000);
    let registry = connected_registry(&[config], None);
    let tool_ref = registry
        .resolve_tool("mcp__slow__wait")
        .expect("slow tool ref");

    let cancelled =
        registry.call_tool_or_cancel(&tool_ref, Value::Object(Default::default()), &|| {
            started_file.exists()
        });

    assert_eq!(cancelled.unwrap_err(), "MCP tool call cancelled");
    assert_eq!(
        registry.server_statuses()[0].state,
        McpServerState::Ready,
        "a restart cut short after a cancel must not fail the server"
    );

    let answered = registry
        .call_tool(&tool_ref, Value::Object(Default::default()))
        .expect("the next call starts the server again and is answered");

    assert_eq!(answered.output, "answered by 3");
    assert_eq!(registry.server_statuses()[0].state, McpServerState::Ready);
}

/// A stdio server whose starts after the first answer `initialize` only
/// after `restart_seconds`, or never, when that is `None`: the server then
/// exits. Its first start blocks in the first `tools/call` it gets and
/// writes `started`, or exits there when `FIRST_CALL` is `crash` in its
/// environment; later ones answer each call, writing their generation to
/// `called` first.
#[cfg(unix)]
fn restarting_server_config(
    dir: &std::path::Path,
    restart_seconds: Option<u32>,
) -> (McpServerConfig, PathBuf, PathBuf) {
    let server = dir.join("restarting_mcp_server.sh");
    let restart = match restart_seconds {
        Some(seconds) => format!("sleep {seconds}"),
        None => "exit 1".to_string(),
    };
    std::fs::write(
            &server,
            format!(
                r#"#!/bin/sh
generation=0
if [ -f "$GENERATION_FILE" ]; then
  IFS= read -r generation < "$GENERATION_FILE"
fi
generation=$((generation + 1))
printf '%s' "$generation" > "$GENERATION_FILE"
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      if [ "$generation" -gt 1 ]; then
        {restart}
      fi
      printf '{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":"2024-11-05","capabilities":{{}},"serverInfo":{{"name":"slow","version":"1"}}}}}}\n'
      ;;
    *'"method":"notifications/initialized"'*)
      ;;
    *'"method":"tools/list"'*)
      printf '{{"jsonrpc":"2.0","id":2,"result":{{"tools":[{{"name":"wait","description":"waits","inputSchema":{{"type":"object","properties":{{}},"required":[]}}}}]}}}}\n'
      ;;
    *'"method":"tools/call"'*)
      if [ "$generation" -eq 1 ] && [ "$FIRST_CALL" = crash ]; then
        exit 1
      elif [ "$generation" -eq 1 ]; then
        printf started > "$STARTED_FILE"
        IFS= read -r ignored
      else
        printf '%s\n' "$generation" >> "$CALLED_FILE"
        id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
        printf '{{"jsonrpc":"2.0","id":%s,"result":{{"content":[{{"type":"text","text":"answered by %s"}}],"isError":false}}}}\n' "$id" "$generation"
      fi
      ;;
  esac
done
"#
            ),
        )
        .expect("write MCP fixture");
    let started_file = dir.join("started");
    let called_file = dir.join("called");
    let mut config = stdio_fixture_config("slow", &server);
    config.env = HashMap::from([
        (
            "GENERATION_FILE".to_string(),
            dir.join("generation").to_string_lossy().into_owned(),
        ),
        (
            "STARTED_FILE".to_string(),
            started_file.to_string_lossy().into_owned(),
        ),
        (
            "CALLED_FILE".to_string(),
            called_file.to_string_lossy().into_owned(),
        ),
    ]);
    config.startup_timeout_ms = Some(10_000);
    (config, started_file, called_file)
}

#[cfg(unix)]
#[test]
fn a_call_cancelled_while_its_server_starts_again_is_never_sent() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let (config, started_file, called_file) = restarting_server_config(temp_dir.path(), Some(3));
    let registry = connected_registry(&[config], None);
    let tool_ref = registry
        .resolve_tool("mcp__slow__wait")
        .expect("slow tool ref");
    // The cancel cuts short the start that follows, so the next call
    // starts the server again first.
    let cancelled =
        registry.call_tool_or_cancel(&tool_ref, Value::Object(Default::default()), &|| {
            started_file.exists()
        });
    assert_eq!(cancelled.unwrap_err(), "MCP tool call cancelled");

    let sent = Instant::now();
    let cancelled =
        registry.call_tool_or_cancel(&tool_ref, Value::Object(Default::default()), &|| {
            sent.elapsed() >= Duration::from_millis(200)
        });

    assert_eq!(cancelled.unwrap_err(), "MCP tool call cancelled");
    assert!(
        sent.elapsed() < Duration::from_secs(2),
        "the cancel waited {:?} for the server to start",
        sent.elapsed()
    );
    assert!(
        !called_file.exists(),
        "a call cancelled before it was sent reached the server"
    );
    assert_eq!(
        registry.server_statuses()[0].state,
        McpServerState::Ready,
        "a start cut short by a cancel must not fail the server"
    );

    let answered = registry
        .call_tool(&tool_ref, Value::Object(Default::default()))
        .expect("the next call starts the server again and is answered");

    // By a start after the one the cancel stopped, the only call any
    // start got.
    let generation = answered
        .output
        .strip_prefix("answered by ")
        .and_then(|generation| generation.parse::<u32>().ok())
        .expect("the generation that answered");
    assert!(generation > 2, "answered by start {generation}");
    assert_eq!(
        std::fs::read_to_string(&called_file).expect("the calls the server got"),
        format!("{generation}\n")
    );
}

#[cfg(unix)]
#[test]
fn a_cancel_stops_the_restart_that_follows_a_crashed_call() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let (mut config, _, called_file) = restarting_server_config(temp_dir.path(), Some(3));
    config
        .env
        .insert("FIRST_CALL".to_string(), "crash".to_string());
    let registry = connected_registry(&[config], None);
    let tool_ref = registry
        .resolve_tool("mcp__slow__wait")
        .expect("slow tool ref");

    let sent = Instant::now();
    let crashed =
        registry.call_tool_or_cancel(&tool_ref, Value::Object(Default::default()), &|| {
            sent.elapsed() >= Duration::from_millis(200)
        });

    assert!(crashed.is_err(), "the server exited in the call");
    assert!(
        sent.elapsed() < Duration::from_secs(2),
        "the cancel waited {:?} for the server to start again",
        sent.elapsed()
    );
    assert_eq!(
        registry.server_statuses()[0].state,
        McpServerState::Ready,
        "a start cut short by a cancel must not fail the server"
    );
    let answered = registry
        .call_tool(&tool_ref, Value::Object(Default::default()))
        .expect("the next call starts the server again and is answered");
    assert!(answered.output.starts_with("answered by "));
    assert!(called_file.exists());
}

#[cfg(unix)]
#[test]
fn a_server_that_cannot_start_again_after_a_cancel_fails_the_next_call() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let (config, started_file, _) = restarting_server_config(temp_dir.path(), None);
    let registry = connected_registry(&[config], None);
    let tool_ref = registry
        .resolve_tool("mcp__slow__wait")
        .expect("slow tool ref");
    let cancelled =
        registry.call_tool_or_cancel(&tool_ref, Value::Object(Default::default()), &|| {
            started_file.exists()
        });
    assert_eq!(cancelled.unwrap_err(), "MCP tool call cancelled");
    assert_eq!(registry.server_statuses()[0].state, McpServerState::Ready);

    let error = registry
        .call_tool(&tool_ref, Value::Object(Default::default()))
        .expect_err("the server cannot start again");

    assert_eq!(
        registry.server_statuses()[0].state,
        McpServerState::Failed { message: error },
        "a server that cannot start again is failed with the reason"
    );
}

#[cfg(unix)]
fn process_is_alive(pid: &str) -> bool {
    std::process::Command::new("/bin/kill")
        .args(["-0", pid])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[test]
fn cancellation_reconnect_policy_is_transport_aware() {
    assert!(should_reconnect_after_mcp_error(
        &McpTransportKind::Stdio,
        "MCP tool call cancelled"
    ));
    assert!(!should_reconnect_after_mcp_error(
        &McpTransportKind::Sse,
        "MCP tool call cancelled"
    ));
}

#[test]
fn registry_call_tool_routes_elicitation_handler_to_transport() {
    struct ElicitingTransport;

    impl McpTransport for ElicitingTransport {
        fn initialize(&self) -> Result<Value, String> {
            Ok(serde_json::json!({"capabilities": {}}))
        }

        fn list_tools(&self, _cursor: Option<&str>) -> Result<Value, String> {
            Ok(serde_json::json!({"tools": []}))
        }

        fn call_tool(&self, _name: &str, _arguments: Value) -> Result<Value, String> {
            Err("handler-aware call expected".to_string())
        }

        fn call_tool_with_elicitation_handler(
            &self,
            _name: &str,
            _arguments: Value,
            handler: Option<&dyn McpElicitationHandler>,
        ) -> Result<Value, String> {
            let response = handler.expect("elicitation handler").handle_elicitation(
                McpElicitationRequest {
                    server_name: "prompts".to_string(),
                    id: "prompt-1".to_string(),
                    mode: McpElicitationMode::Form,
                    message: "Enter token".to_string(),
                    url: None,
                    requested_schema: Some(serde_json::json!({"type":"object"})),
                },
            )?;
            assert_eq!(
                response,
                McpElicitationResponse::accept(serde_json::json!({"token":"abc"}))
            );
            Ok(serde_json::json!({
                "content": [{"type": "text", "text": "accepted"}],
                "isError": false
            }))
        }

        fn list_resources(&self, _cursor: Option<&str>) -> Result<Value, String> {
            Ok(serde_json::json!({"resources": []}))
        }

        fn list_resource_templates(&self, _cursor: Option<&str>) -> Result<Value, String> {
            Ok(serde_json::json!({"resourceTemplates": []}))
        }

        fn read_resource(&self, _uri: &str) -> Result<Value, String> {
            Ok(serde_json::json!({"contents": []}))
        }
    }

    struct AcceptingHandler;

    impl McpElicitationHandler for AcceptingHandler {
        fn handle_elicitation(
            &self,
            request: McpElicitationRequest,
        ) -> Result<McpElicitationResponse, String> {
            assert_eq!(request.message, "Enter token");
            Ok(McpElicitationResponse::accept(
                serde_json::json!({"token":"abc"}),
            ))
        }
    }

    let tool = McpTool {
        server: "prompts".to_string(),
        name: "authorize".to_string(),
        schema_name: "mcp__prompts__authorize".to_string(),
        description: None,
        input_schema: serde_json::json!({"type": "object"}),
        read_only: false,
    };
    let registry = McpRegistry::from_inner(McpRegistryInner {
        clients: HashMap::from([(
            "prompts".to_string(),
            Arc::new(McpClient {
                config: McpServerConfig {
                    name: "prompts".to_string(),
                    ..Default::default()
                },
                server_name: "prompts".to_string(),
                credentials_path: None,
                capabilities: McpServerCapabilities::default(),
                reconnect_failure: Mutex::default(),
                stop: Arc::default(),
                transport: Mutex::new(Arc::new(ElicitingTransport)),
            }),
        )]),
        tools: vec![tool.clone()],
        lookup: HashMap::from([(
            tool.schema_name.clone(),
            McpToolRef {
                server: tool.server,
                tool: tool.name,
                schema_name: tool.schema_name,
            },
        )]),
        ..Default::default()
    });
    let tool_ref = registry
        .resolve_tool("mcp__prompts__authorize")
        .expect("tool ref");

    let result = registry
        .call_tool_with_elicitation_handler(
            &tool_ref,
            serde_json::json!({}),
            Some(&AcceptingHandler),
        )
        .expect("tool result");

    assert_eq!(result.output, "accepted");
}

#[test]
fn registry_aggregates_mcp_resource_list_errors_without_losing_successes() {
    struct ResourceListTransport {
        result: Result<Value, String>,
    }

    impl McpTransport for ResourceListTransport {
        fn initialize(&self) -> Result<Value, String> {
            Ok(serde_json::json!({"capabilities": {"resources": {}}}))
        }

        fn list_tools(&self, _cursor: Option<&str>) -> Result<Value, String> {
            Ok(serde_json::json!({"tools": []}))
        }

        fn call_tool(&self, _name: &str, _arguments: Value) -> Result<Value, String> {
            Err("not used".to_string())
        }

        fn list_resources(&self, _cursor: Option<&str>) -> Result<Value, String> {
            self.result.clone()
        }

        fn list_resource_templates(&self, _cursor: Option<&str>) -> Result<Value, String> {
            Ok(serde_json::json!({"resourceTemplates": []}))
        }

        fn read_resource(&self, _uri: &str) -> Result<Value, String> {
            Err("not used".to_string())
        }
    }

    let registry = McpRegistry::from_resource_transports_for_test([
        (
            "notes".to_string(),
            Box::new(ResourceListTransport {
                result: Ok(serde_json::json!({
                    "resources": [
                        {
                            "uri": "memo://orca/one",
                            "name": "memo one",
                            "description": "A test memo",
                            "mimeType": "text/plain"
                        }
                    ]
                })),
            }) as Box<dyn McpTransport>,
        ),
        (
            "broken".to_string(),
            Box::new(ResourceListTransport {
                result: Err("resources/list timed out".to_string()),
            }) as Box<dyn McpTransport>,
        ),
    ]);

    let listing = registry.list_resources_with_errors(None);

    assert_eq!(listing.resources.len(), 1);
    assert_eq!(listing.resources[0].server, "notes");
    assert_eq!(listing.resources[0].uri, "memo://orca/one");
    assert_eq!(
        listing.errors,
        vec!["broken: resources/list timed out".to_string()]
    );

    let single_server_error = registry
        .list_resources(Some("broken"))
        .expect_err("single-server resource list should stay strict");
    assert_eq!(single_server_error, "resources/list timed out");
}

#[test]
fn registry_includes_initialization_errors_in_all_server_resource_listing() {
    struct ResourceListTransport;

    impl McpTransport for ResourceListTransport {
        fn initialize(&self) -> Result<Value, String> {
            Ok(serde_json::json!({"capabilities": {"resources": {}}}))
        }

        fn list_tools(&self, _cursor: Option<&str>) -> Result<Value, String> {
            Ok(serde_json::json!({"tools": []}))
        }

        fn call_tool(&self, _name: &str, _arguments: Value) -> Result<Value, String> {
            Err("not used".to_string())
        }

        fn list_resources(&self, _cursor: Option<&str>) -> Result<Value, String> {
            Ok(serde_json::json!({
                "resources": [
                    {
                        "uri": "memo://orca/one",
                        "name": "memo one",
                        "description": "A test memo",
                        "mimeType": "text/plain"
                    }
                ]
            }))
        }

        fn list_resource_templates(&self, _cursor: Option<&str>) -> Result<Value, String> {
            Ok(serde_json::json!({"resourceTemplates": []}))
        }

        fn read_resource(&self, _uri: &str) -> Result<Value, String> {
            Err("not used".to_string())
        }
    }

    let registry = McpRegistry::from_resource_transports_for_test([(
        "notes".to_string(),
        Box::new(ResourceListTransport) as Box<dyn McpTransport>,
    )])
    .with_registry_errors_for_test(vec![
        "failed to start MCP server 'broken': boom".to_string(),
    ]);

    let listing = registry.list_resources_with_errors(None);

    assert_eq!(listing.resources.len(), 1);
    assert_eq!(listing.resources[0].server, "notes");
    assert_eq!(
        listing.errors,
        vec!["failed to start MCP server 'broken': boom".to_string()]
    );

    let single_server_listing = registry
        .list_resources(Some("notes"))
        .expect("single-server resource listing");
    assert_eq!(single_server_listing.len(), 1);
}

#[test]
fn registry_aggregates_mcp_resource_template_errors_without_losing_successes() {
    struct TemplateListTransport {
        result: Result<Value, String>,
    }

    impl McpTransport for TemplateListTransport {
        fn initialize(&self) -> Result<Value, String> {
            Ok(serde_json::json!({"capabilities": {"resources": {}}}))
        }

        fn list_tools(&self, _cursor: Option<&str>) -> Result<Value, String> {
            Ok(serde_json::json!({"tools": []}))
        }

        fn call_tool(&self, _name: &str, _arguments: Value) -> Result<Value, String> {
            Err("not used".to_string())
        }

        fn list_resources(&self, _cursor: Option<&str>) -> Result<Value, String> {
            Ok(serde_json::json!({"resources": []}))
        }

        fn read_resource(&self, _uri: &str) -> Result<Value, String> {
            Err("not used".to_string())
        }

        fn list_resource_templates(&self, _cursor: Option<&str>) -> Result<Value, String> {
            self.result.clone()
        }
    }

    let registry = McpRegistry::from_resource_transports_for_test([
        (
            "docs".to_string(),
            Box::new(TemplateListTransport {
                result: Ok(serde_json::json!({
                    "resourceTemplates": [
                        {
                            "uriTemplate": "file:///{path}",
                            "name": "workspace file",
                            "description": "A file exposed by path",
                            "mimeType": "text/plain"
                        }
                    ]
                })),
            }) as Box<dyn McpTransport>,
        ),
        (
            "broken".to_string(),
            Box::new(TemplateListTransport {
                result: Err("resources/templates/list timed out".to_string()),
            }) as Box<dyn McpTransport>,
        ),
    ]);

    let listing = registry.list_resource_templates_with_errors(None);

    assert_eq!(listing.resource_templates.len(), 1);
    assert_eq!(listing.resource_templates[0].server, "docs");
    assert_eq!(listing.resource_templates[0].uri_template, "file:///{path}");
    assert_eq!(
        listing.errors,
        vec!["broken: resources/templates/list timed out".to_string()]
    );

    let single_server_error = registry
        .list_resource_templates(Some("broken"))
        .expect_err("single-server resource template list should stay strict");
    assert_eq!(single_server_error, "resources/templates/list timed out");
}

#[test]
fn registry_includes_initialization_errors_in_all_server_resource_template_listing() {
    struct TemplateListTransport;

    impl McpTransport for TemplateListTransport {
        fn initialize(&self) -> Result<Value, String> {
            Ok(serde_json::json!({"capabilities": {"resources": {}}}))
        }

        fn list_tools(&self, _cursor: Option<&str>) -> Result<Value, String> {
            Ok(serde_json::json!({"tools": []}))
        }

        fn call_tool(&self, _name: &str, _arguments: Value) -> Result<Value, String> {
            Err("not used".to_string())
        }

        fn list_resources(&self, _cursor: Option<&str>) -> Result<Value, String> {
            Ok(serde_json::json!({"resources": []}))
        }

        fn read_resource(&self, _uri: &str) -> Result<Value, String> {
            Err("not used".to_string())
        }

        fn list_resource_templates(&self, _cursor: Option<&str>) -> Result<Value, String> {
            Ok(serde_json::json!({
                "resourceTemplates": [
                    {
                        "uriTemplate": "file:///{path}",
                        "name": "workspace file",
                        "description": "A file exposed by path",
                        "mimeType": "text/plain"
                    }
                ]
            }))
        }
    }

    let registry = McpRegistry::from_resource_transports_for_test([(
        "docs".to_string(),
        Box::new(TemplateListTransport) as Box<dyn McpTransport>,
    )])
    .with_registry_errors_for_test(vec![
        "failed to start MCP server 'broken': boom".to_string(),
    ]);

    let listing = registry.list_resource_templates_with_errors(None);

    assert_eq!(listing.resource_templates.len(), 1);
    assert_eq!(listing.resource_templates[0].server, "docs");
    assert_eq!(
        listing.errors,
        vec!["failed to start MCP server 'broken': boom".to_string()]
    );

    let single_server_listing = registry
        .list_resource_templates(Some("docs"))
        .expect("single-server resource template listing");
    assert_eq!(single_server_listing.len(), 1);
}

#[cfg(unix)]
#[test]
fn all_server_resource_listing_skips_servers_without_resource_capability() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let resources_server = temp_dir.path().join("resources_server.sh");
    std::fs::write(
            &resources_server,
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{"resources":{}},"serverInfo":{"name":"resources","version":"1"}}}\n'
      ;;
    *'"method":"notifications/initialized"'*)
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[]}}\n'
      ;;
    *'"method":"resources/list"'*)
      printf '{"jsonrpc":"2.0","id":3,"result":{"resources":[{"uri":"memo://orca/one","name":"memo one","description":"A test memo","mimeType":"text/plain"}]}}\n'
      ;;
  esac
done
"#,
        )
        .expect("write resource MCP fixture");
    let tools_only_server = temp_dir.path().join("tools_only_server.sh");
    std::fs::write(
            &tools_only_server,
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"tools-only","version":"1"}}}\n'
      ;;
    *'"method":"notifications/initialized"'*)
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[]}}\n'
      ;;
    *'"method":"resources/list"'*)
      printf '{"jsonrpc":"2.0","id":3,"error":{"code":-32601,"message":"resources/list unsupported"}}\n'
      ;;
  esac
done
"#,
        )
        .expect("write tools-only MCP fixture");
    for server in [&resources_server, &tools_only_server] {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(server).expect("metadata").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(server, permissions).expect("chmod MCP fixture");
    }

    let registry = connected_registry(
        &[
            stdio_fixture_config("resources", &resources_server),
            stdio_fixture_config("tools_only", &tools_only_server),
        ],
        None,
    );

    let listing = registry.list_resources_with_errors(None);

    assert_eq!(listing.resources.len(), 1);
    assert_eq!(listing.resources[0].server, "resources");
    assert_eq!(listing.resources[0].uri, "memo://orca/one");
    assert!(
        listing.errors.is_empty(),
        "tools-only server should be skipped, got {:?}",
        listing.errors
    );

    let explicit_error = registry
        .list_resources(Some("tools_only"))
        .expect_err("explicit server filter should still call the selected server");
    assert!(explicit_error.contains("resources/list unsupported"));
}

#[cfg(unix)]
#[test]
fn all_server_resource_template_listing_skips_servers_without_resource_capability() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let resources_server = temp_dir.path().join("resource_templates_server.sh");
    std::fs::write(
            &resources_server,
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{"resources":{}},"serverInfo":{"name":"resources","version":"1"}}}\n'
      ;;
    *'"method":"notifications/initialized"'*)
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[]}}\n'
      ;;
    *'"method":"resources/templates/list"'*)
      printf '{"jsonrpc":"2.0","id":3,"result":{"resourceTemplates":[{"uriTemplate":"file:///{path}","name":"workspace file","description":"A file exposed by path","mimeType":"text/plain"}]}}\n'
      ;;
  esac
done
"#,
        )
        .expect("write resource templates MCP fixture");
    let tools_only_server = temp_dir.path().join("tools_only_templates_server.sh");
    std::fs::write(
            &tools_only_server,
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"tools-only","version":"1"}}}\n'
      ;;
    *'"method":"notifications/initialized"'*)
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[]}}\n'
      ;;
    *'"method":"resources/templates/list"'*)
      printf '{"jsonrpc":"2.0","id":3,"error":{"code":-32601,"message":"resources/templates/list unsupported"}}\n'
      ;;
  esac
done
"#,
        )
        .expect("write tools-only MCP fixture");
    for server in [&resources_server, &tools_only_server] {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(server).expect("metadata").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(server, permissions).expect("chmod MCP fixture");
    }

    let registry = connected_registry(
        &[
            stdio_fixture_config("resources", &resources_server),
            stdio_fixture_config("tools_only", &tools_only_server),
        ],
        None,
    );

    let listing = registry.list_resource_templates_with_errors(None);

    assert_eq!(listing.resource_templates.len(), 1);
    assert_eq!(listing.resource_templates[0].server, "resources");
    assert_eq!(listing.resource_templates[0].uri_template, "file:///{path}");
    assert!(
        listing.errors.is_empty(),
        "tools-only server should be skipped, got {:?}",
        listing.errors
    );

    let explicit_error = registry
        .list_resource_templates(Some("tools_only"))
        .expect_err("explicit server filter should still call the selected server");
    assert!(explicit_error.contains("resources/templates/list unsupported"));
}

#[cfg(unix)]
#[test]
fn registry_lists_and_reads_mcp_resources_from_stdio_server() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let server = temp_dir.path().join("resource_mcp_server.sh");
    std::fs::write(
            &server,
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{"resources":{}},"serverInfo":{"name":"resources","version":"1"}}}\n'
      ;;
    *'"method":"notifications/initialized"'*)
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[]}}\n'
      ;;
    *'"method":"resources/list"'*)
      printf '{"jsonrpc":"2.0","id":3,"result":{"resources":[{"uri":"memo://orca/one","name":"memo one","description":"A test memo","mimeType":"text/plain"}]}}\n'
      ;;
    *'"method":"resources/read"'*)
      printf '{"jsonrpc":"2.0","id":4,"result":{"contents":[{"uri":"memo://orca/one","mimeType":"text/plain","text":"resource body"}]}}\n'
      ;;
  esac
done
"#,
        )
        .expect("write MCP fixture");
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&server).expect("metadata").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&server, permissions).expect("chmod MCP fixture");
    }

    let registry = connected_registry(&[stdio_fixture_config("resources", &server)], None);
    assert!(
        registry.errors().is_empty(),
        "registry errors: {:?}",
        registry.errors()
    );

    let resources = registry
        .list_resources(None)
        .expect("list all MCP resources");
    assert_eq!(resources.len(), 1);
    assert_eq!(resources[0].server, "resources");
    assert_eq!(resources[0].uri, "memo://orca/one");
    assert_eq!(resources[0].name, "memo one");
    assert_eq!(resources[0].mime_type.as_deref(), Some("text/plain"));

    let content = registry
        .read_resource("resources", "memo://orca/one")
        .expect("read MCP resource");
    assert_eq!(content.contents.len(), 1);
    assert_eq!(content.contents[0].text.as_deref(), Some("resource body"));
    assert_eq!(content.contents[0].mime_type.as_deref(), Some("text/plain"));
}

#[cfg(unix)]
#[test]
fn registry_lists_mcp_resource_templates_from_stdio_server() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let server = temp_dir.path().join("resource_template_mcp_server.sh");
    std::fs::write(
            &server,
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{"resources":{}},"serverInfo":{"name":"templates","version":"1"}}}\n'
      ;;
    *'"method":"notifications/initialized"'*)
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[]}}\n'
      ;;
    *'"method":"resources/templates/list"'*)
      printf '{"jsonrpc":"2.0","id":3,"result":{"resourceTemplates":[{"uriTemplate":"file:///{path}","name":"workspace file","description":"A file exposed by path","mimeType":"text/plain"}]}}\n'
      ;;
  esac
done
"#,
        )
        .expect("write MCP fixture");
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&server).expect("metadata").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&server, permissions).expect("chmod MCP fixture");
    }

    let registry = connected_registry(&[stdio_fixture_config("templates", &server)], None);
    assert!(
        registry.errors().is_empty(),
        "registry errors: {:?}",
        registry.errors()
    );

    let templates = registry
        .list_resource_templates(None)
        .expect("list all MCP resource templates");
    assert_eq!(templates.len(), 1);
    assert_eq!(templates[0].server, "templates");
    assert_eq!(templates[0].uri_template, "file:///{path}");
    assert_eq!(templates[0].name, "workspace file");
    assert_eq!(templates[0].mime_type.as_deref(), Some("text/plain"));
}

#[cfg(unix)]
#[test]
fn stdio_client_reconnects_after_timed_out_tool_call() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let state_dir = temp_dir.path().join("state");
    std::fs::create_dir_all(&state_dir).expect("state dir");
    let server = temp_dir.path().join("reconnecting_mcp_server.sh");
    std::fs::write(
            &server,
            r#"#!/bin/sh
state_dir="$1"
run_file="$state_dir/run-count"
call_file="$state_dir/call-count"
run_count=0
if [ -f "$run_file" ]; then
  run_count=$(cat "$run_file")
fi
run_count=$((run_count + 1))
printf '%s' "$run_count" > "$run_file"
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"slow","version":"1"}}}\n'
      ;;
    *'"method":"notifications/initialized"'*)
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"wait","description":"waits","inputSchema":{"type":"object","properties":{},"required":[]}}]}}\n'
      ;;
    *'"method":"tools/call"'*)
      call_count=0
      if [ -f "$call_file" ]; then
        call_count=$(cat "$call_file")
      fi
      call_count=$((call_count + 1))
      printf '%s' "$call_count" > "$call_file"
      if [ "$call_count" -eq 1 ]; then
        sleep 5
        printf '{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"too late"}],"isError":false}}\n'
      else
        printf '{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"reconnected"}],"isError":false}}\n'
      fi
      ;;
  esac
done
"#,
        )
        .expect("write MCP fixture");
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&server).expect("metadata").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&server, permissions).expect("chmod MCP fixture");
    }
    let registry = connected_registry(
        &[McpServerConfig {
            name: "slow".to_string(),
            transport: orca_core::mcp_types::McpTransportKind::Stdio,
            command: Some("/bin/sh".to_string()),
            args: vec![
                server.to_string_lossy().into_owned(),
                state_dir.to_string_lossy().into_owned(),
            ],
            url: None,
            env: Default::default(),
            headers: Default::default(),
            disabled: false,
            capabilities: Default::default(),
            startup_timeout_ms: Some(STDIO_TEST_STARTUP_TIMEOUT_MS),
            tool_timeout_ms: Some(100),
            ..Default::default()
        }],
        None,
    );
    assert!(
        registry.errors().is_empty(),
        "registry errors: {:?}",
        registry.errors()
    );
    let tool_ref = registry
        .resolve_tool("mcp__slow__wait")
        .expect("registered MCP tool");

    let first = registry.call_tool(&tool_ref, serde_json::json!({}));
    assert!(
        first
            .unwrap_err()
            .contains("MCP request 'tools/call' timed out after 100ms")
    );

    let second = registry
        .call_tool(&tool_ref, serde_json::json!({}))
        .expect("second call should reconnect");

    assert_eq!(second.output, "reconnected");
    let runs = std::fs::read_to_string(state_dir.join("run-count")).expect("run count");
    assert_eq!(runs, "2");
}

/// Builds a valid RGBA PNG whose pixel rows are stored uncompressed
/// (deflate "stored" blocks), so its size tracks `width * height` the way
/// a detailed screenshot's does.
#[cfg(unix)]
fn uncompressed_png(width: u32, height: u32) -> Vec<u8> {
    fn crc32(bytes: &[u8]) -> u32 {
        let table = (0..256u32)
            .map(|entry| {
                (0..8).fold(entry, |crc, _| {
                    if crc & 1 == 1 {
                        0xEDB8_8320 ^ (crc >> 1)
                    } else {
                        crc >> 1
                    }
                })
            })
            .collect::<Vec<_>>();
        !bytes.iter().fold(!0u32, |crc, byte| {
            table[((crc ^ u32::from(*byte)) & 0xFF) as usize] ^ (crc >> 8)
        })
    }
    fn push_chunk(png: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        let length = u32::try_from(data.len()).expect("PNG chunk length");
        png.extend_from_slice(&length.to_be_bytes());
        let start = png.len();
        png.extend_from_slice(kind);
        png.extend_from_slice(data);
        let crc = crc32(&png[start..]);
        png.extend_from_slice(&crc.to_be_bytes());
    }

    let mut rows = Vec::new();
    for y in 0..height {
        rows.push(0); // filter type: none
        for x in 0..width {
            rows.extend_from_slice(&[x as u8, y as u8, (x ^ y) as u8, 0xFF]);
        }
    }
    let mut zlib = vec![0x78, 0x01];
    let blocks = rows.chunks(usize::from(u16::MAX)).collect::<Vec<_>>();
    for (index, block) in blocks.iter().enumerate() {
        // BFINAL on the last block; BTYPE 00 means stored.
        zlib.push(u8::from(index + 1 == blocks.len()));
        let length = u16::try_from(block.len()).expect("stored block length");
        zlib.extend_from_slice(&length.to_le_bytes());
        zlib.extend_from_slice(&(!length).to_le_bytes());
        zlib.extend_from_slice(block);
    }
    let (a, b) = rows.iter().fold((1u32, 0u32), |(a, b), byte| {
        let a = (a + u32::from(*byte)) % 65_521;
        (a, (b + a) % 65_521)
    });
    zlib.extend_from_slice(&((b << 16) | a).to_be_bytes());

    let mut header = Vec::new();
    header.extend_from_slice(&width.to_be_bytes());
    header.extend_from_slice(&height.to_be_bytes());
    // 8-bit RGBA, deflate, adaptive filtering, no interlace.
    header.extend_from_slice(&[8, 6, 0, 0, 0]);
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    push_chunk(&mut png, b"IHDR", &header);
    push_chunk(&mut png, b"IDAT", &zlib);
    push_chunk(&mut png, b"IEND", &[]);
    png
}

#[cfg(unix)]
#[test]
fn a_screenshot_sized_png_arrives_through_the_stdio_transport() {
    use base64::Engine as _;

    let temp_dir = tempfile::tempdir().expect("temp dir");
    let server = temp_dir.path().join("screenshot_mcp_server.sh");
    let response_file = temp_dir.path().join("screenshot-response.jsonl");
    // 1024x768 RGBA stored uncompressed: about 3 MiB, the size of a
    // detailed Retina screenshot.
    let png = uncompressed_png(1024, 768);
    assert!(png.len() > 3 * 1024 * 1024, "PNG is {} bytes", png.len());
    let data = base64::engine::general_purpose::STANDARD.encode(&png);
    let response = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 3,
        "result": {
            "content": [
                {"type": "text", "text": "screenshot taken"},
                {"type": "image", "data": &data, "mimeType": "image/png"}
            ],
            "isError": false
        }
    });
    std::fs::write(&response_file, format!("{response}\n")).expect("write MCP response");
    std::fs::write(
            &server,
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"screen","version":"1"}}}\n'
      ;;
    *'"method":"notifications/initialized"'*)
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"capture","description":"captures the screen","inputSchema":{"type":"object","properties":{},"required":[]}}]}}\n'
      ;;
    *'"method":"tools/call"'*)
      cat "$1"
      ;;
  esac
done
"#,
        )
        .expect("write MCP fixture");
    let mut config = stdio_fixture_config("screen", &server);
    config
        .args
        .push(response_file.to_string_lossy().into_owned());
    config.tool_timeout_ms = Some(STDIO_TEST_STARTUP_TIMEOUT_MS);
    let registry = connected_registry(&[config], None);
    assert!(registry.errors().is_empty(), "{:?}", registry.errors());
    let tool_ref = registry
        .resolve_tool("mcp__screen__capture")
        .expect("screen capture tool ref");

    let result = registry
        .call_tool(&tool_ref, serde_json::json!({}))
        .expect("a screenshot-sized image fits in one MCP response");

    assert_eq!(result.output, "screenshot taken\n[1 image attached]");
    assert_eq!(result.images.len(), 1);
    assert!(matches!(
        &result.images[0].source,
        orca_core::conversation::ImageSource::Base64 { media_type, data: sent }
            if media_type == "image/png" && *sent == data
    ));
}

#[cfg(unix)]
#[test]
fn stdio_client_reconnects_after_an_oversized_tool_response() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let state_dir = temp_dir.path().join("state");
    std::fs::create_dir_all(&state_dir).expect("state dir");
    let oversized = temp_dir.path().join("oversized-response.jsonl");
    let response = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 3,
        "result": {
            "content": [{
                "type": "text",
                "text": "x".repeat(crate::transport::MAX_STDIO_RESPONSE_LINE_BYTES)
            }],
            "isError": false
        }
    });
    std::fs::write(&oversized, format!("{response}\n")).expect("write oversized response");
    let server = temp_dir.path().join("oversized_mcp_server.sh");
    std::fs::write(
            &server,
            r#"#!/bin/sh
state_dir="$1"
run_file="$state_dir/run-count"
run_count=0
if [ -f "$run_file" ]; then
  run_count=$(cat "$run_file")
fi
run_count=$((run_count + 1))
printf '%s' "$run_count" > "$run_file"
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"oversized","version":"1"}}}\n'
      ;;
    *'"method":"notifications/initialized"'*)
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"capture","description":"captures the screen","inputSchema":{"type":"object","properties":{},"required":[]}}]}}\n'
      ;;
    *'"method":"tools/call"'*)
      if [ "$run_count" -eq 1 ]; then
        cat "$2"
      else
        printf '{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"reconnected"}],"isError":false}}\n'
      fi
      ;;
  esac
done
"#,
        )
        .expect("write MCP fixture");
    let mut config = stdio_fixture_config("oversized", &server);
    config.args.extend([
        state_dir.to_string_lossy().into_owned(),
        oversized.to_string_lossy().into_owned(),
    ]);
    config.tool_timeout_ms = Some(STDIO_TEST_STARTUP_TIMEOUT_MS);
    let registry = connected_registry(&[config], None);
    assert!(registry.errors().is_empty(), "{:?}", registry.errors());
    let tool_ref = registry
        .resolve_tool("mcp__oversized__capture")
        .expect("registered MCP tool");

    let first = registry.call_tool(&tool_ref, serde_json::json!({}));
    let second = registry.call_tool(&tool_ref, serde_json::json!({}));

    let first = first.expect_err("an oversized response fails its call");
    assert!(
        first.contains("MCP response exceeded maximum line size"),
        "unexpected oversized response error: {first}"
    );
    assert_eq!(
        second
            .expect("the next call must reach a reconnected server")
            .output,
        "reconnected"
    );
    let runs = std::fs::read_to_string(state_dir.join("run-count")).expect("run count");
    assert_eq!(runs, "2");
}

/// A stdio server that lists the tools in `<dir>/<name>/tools.json`, and
/// never answers a tool call. Each time it starts, it reads its tools
/// and the seconds in `<dir>/<name>/delay`, if there is one, adds its
/// pid to `<dir>/<name>/starts`, and to `<dir>/<name>/overlaps` too when
/// the start before it is still running, and then waits that long
/// before it answers anything, `initialize` included. Each message it
/// gets is added to `<dir>/<name>/requests`.
#[cfg(unix)]
fn listing_server_config(name: &str, dir: &std::path::Path, tools: &str) -> McpServerConfig {
    let state = dir.join(name);
    std::fs::create_dir_all(&state).expect("state dir");
    std::fs::write(state.join("tools.json"), tools).expect("write the tool list");
    let script = dir.join(format!("{name}.sh"));
    std::fs::write(
            &script,
            r#"#!/bin/sh
state_dir="$1"
tools=$(cat "$state_dir/tools.json")
delay=$(cat "$state_dir/delay" 2>/dev/null)
previous=$(tail -n 1 "$state_dir/starts" 2>/dev/null)
if [ -n "$previous" ] && kill -0 "$previous" 2>/dev/null; then
  printf '%s\n' "$$" >> "$state_dir/overlaps"
fi
printf '%s\n' "$$" >> "$state_dir/starts"
if [ -n "$delay" ]; then
  sleep "$delay"
fi
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$state_dir/requests"
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"listing","version":"1"}}}\n'
      ;;
    *'"method":"notifications/initialized"'*)
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":%s}}\n' "$tools"
      ;;
  esac
done
"#,
        )
        .expect("write MCP fixture");
    let mut config = stdio_fixture_config(name, &script);
    config.args.push(state.to_string_lossy().into_owned());
    config
}

#[cfg(unix)]
fn starts(dir: &std::path::Path, name: &str) -> usize {
    start_pids(dir, name).len()
}

/// The pid of each start of the listing server `name`, in order.
#[cfg(unix)]
fn start_pids(dir: &std::path::Path, name: &str) -> Vec<String> {
    std::fs::read_to_string(dir.join(name).join("starts"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

/// The pid of each start of the listing server `name` that found the
/// start before it still running.
#[cfg(unix)]
fn overlapping_starts(dir: &std::path::Path, name: &str) -> Vec<String> {
    std::fs::read_to_string(dir.join(name).join("overlaps"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

/// Has the listing server `name` wait `seconds` before it answers, from
/// its next start on.
#[cfg(unix)]
fn delay_starts(dir: &std::path::Path, name: &str, seconds: u32) {
    std::fs::write(dir.join(name).join("delay"), seconds.to_string())
        .expect("write the start delay");
}

/// Waits up to ten seconds for `condition`, and fails the test, saying
/// it was waiting for `what`, if it never holds.
#[cfg(unix)]
fn wait_for(what: &str, condition: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// `name` in `state`, with its prompts listed, and nothing gone wrong
/// when it was last connected.
fn status(name: &str, state: McpServerState) -> McpServerStatus {
    McpServerStatus {
        name: name.to_string(),
        state,
        prompts_error: None,
        errors: Vec::new(),
    }
}

/// `name`, which `error` kept from connecting.
fn failed(name: &str, error: &str) -> McpServerStatus {
    McpServerStatus {
        errors: vec![error.to_string()],
        ..status(
            name,
            McpServerState::Failed {
                message: error.to_string(),
            },
        )
    }
}

fn schema_names(registry: &McpRegistry) -> Vec<String> {
    registry
        .tools()
        .into_iter()
        .map(|tool| tool.schema_name)
        .collect()
}

/// A stdio server whose command does not exist.
fn missing_server_config(name: &str, dir: &std::path::Path) -> McpServerConfig {
    McpServerConfig {
        name: name.to_string(),
        command: Some(dir.join("missing-server").to_string_lossy().into_owned()),
        startup_timeout_ms: Some(STDIO_TEST_STARTUP_TIMEOUT_MS),
        ..Default::default()
    }
}

#[cfg(unix)]
#[test]
fn reconnecting_a_server_replaces_only_its_tools() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let first = listing_server_config(
        "first",
        temp_dir.path(),
        r#"[{"name":"old_tool","inputSchema":{"type":"object"}}]"#,
    );
    let second = listing_server_config(
        "second",
        temp_dir.path(),
        r#"[{"name":"stable","inputSchema":{"type":"object"}}]"#,
    );
    let registry = connected_registry(&[first, second], None);
    assert!(registry.errors().is_empty(), "{:?}", registry.errors());
    assert_eq!(
        schema_names(&registry),
        ["mcp__first__old_tool", "mcp__second__stable"]
    );
    // A tool registry holds a clone; it must see the new tools too.
    let held = registry.clone();
    std::fs::write(
        temp_dir.path().join("first").join("tools.json"),
        r#"[{"name":"new_tool","inputSchema":{"type":"object"}}]"#,
    )
    .expect("change the first server's tools");

    registry
        .reconnect_server("first")
        .expect("reconnect the first server");

    assert_eq!(
        schema_names(&held),
        ["mcp__first__new_tool", "mcp__second__stable"]
    );
    assert!(held.resolve_tool("mcp__first__old_tool").is_none());
    assert!(held.resolve_tool("mcp__first__new_tool").is_some());
    assert!(held.resolve_tool("mcp__second__stable").is_some());
    assert_eq!(starts(temp_dir.path(), "first"), 2);
    assert_eq!(
        starts(temp_dir.path(), "second"),
        1,
        "the second server keeps its connection"
    );
    assert_eq!(
        held.server_statuses(),
        [
            status("first", McpServerState::Ready),
            status("second", McpServerState::Ready),
        ]
    );
}

#[test]
fn a_failed_server_is_listed_with_its_error() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let registry = connected_registry(&[missing_server_config("broken", temp_dir.path())], None);

    let errors = registry.errors();
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(
        errors[0].starts_with("failed to start MCP server 'broken'"),
        "{errors:?}"
    );
    assert_eq!(registry.server_statuses(), [failed("broken", &errors[0])]);

    let error = registry
        .reconnect_server("broken")
        .expect_err("the server's command is still missing");
    assert!(
        error.starts_with("failed to start MCP server 'broken'"),
        "{error}"
    );
    assert_eq!(registry.server_statuses(), [failed("broken", &error)]);
    assert!(registry.tools().is_empty());
}

#[test]
fn a_server_that_needs_login_reports_needs_login() {
    use crate::oauth::test_server::{OAuthTestBehavior, OAuthTestServer};
    use orca_core::config::mcp_credentials::{McpCredential, save_mcp_credential};

    let server = OAuthTestServer::start(OAuthTestBehavior {
        accepted_tokens: vec!["at-stored".to_string()],
        ..Default::default()
    });
    let home = tempfile::tempdir().expect("temp dir");
    let credentials = home.path().join("mcp-credentials.json");
    let registry = connected_registry(&[server.config("docs")], Some(credentials.clone()));

    assert_eq!(
        registry.server_statuses(),
        [McpServerStatus {
            errors: vec![LOGIN_REQUIRED.to_string()],
            ..status("docs", McpServerState::NeedsLogin)
        }]
    );
    assert_eq!(registry.errors(), [LOGIN_REQUIRED]);
    assert!(registry.tools().is_empty());

    // After `orca mcp login`, a reconnect sends the stored token.
    save_mcp_credential(
        &credentials,
        "docs",
        &McpCredential {
            server_url: server.mcp_url(),
            access_token: "at-stored".to_string(),
            refresh_token: None,
            expires_at: None,
            token_endpoint: format!("{}/token", server.url()),
            client_id: "configured-client".to_string(),
            resource: server.mcp_url(),
            scope: None,
        },
    )
    .expect("store a login");
    registry
        .reconnect_server("docs")
        .expect("reconnect after logging in");

    assert_eq!(
        registry.server_statuses(),
        [status("docs", McpServerState::Ready)]
    );
    assert!(registry.errors().is_empty(), "{:?}", registry.errors());
    assert_eq!(schema_names(&registry), ["mcp__docs__echo"]);
}

#[test]
fn a_call_that_finds_the_login_gone_marks_the_server_as_needing_login() {
    use crate::oauth::test_server::{OAuthTestBehavior, OAuthTestServer};
    use orca_core::config::mcp_credentials::{McpCredential, save_mcp_credential};

    let server = OAuthTestServer::start(OAuthTestBehavior {
        accepted_tokens: vec!["at-stored".to_string()],
        refuse_refresh: true,
        ..Default::default()
    });
    let home = tempfile::tempdir().expect("temp dir");
    let credentials = home.path().join("mcp-credentials.json");
    save_mcp_credential(
        &credentials,
        "docs",
        &McpCredential {
            server_url: server.mcp_url(),
            access_token: "at-stored".to_string(),
            refresh_token: Some("rt-1".to_string()),
            expires_at: None,
            token_endpoint: format!("{}/token", server.url()),
            client_id: "configured-client".to_string(),
            resource: server.mcp_url(),
            scope: None,
        },
    )
    .expect("store a login");
    let registry = connected_registry(&[server.config("docs")], Some(credentials));
    assert_eq!(
        registry.server_statuses(),
        [status("docs", McpServerState::Ready)]
    );

    // The login is revoked, and the refresh token is turned away too.
    server.revoke_tokens();
    let echo = registry
        .resolve_tool("mcp__docs__echo")
        .expect("the echo tool");
    let error = registry
        .call_tool(&echo, serde_json::json!({}))
        .expect_err("the server turns the token away");

    assert_eq!(
        error,
        "MCP server requires login: run 'orca mcp login docs', or log in from /mcp"
    );
    assert_eq!(
        registry.server_statuses(),
        [status("docs", McpServerState::NeedsLogin)]
    );
}

/// What a request to the OAuth test server, as "docs", fails with once
/// its login is gone.
const LOGIN_REQUIRED: &str =
    "MCP server requires login: run 'orca mcp login docs', or log in from /mcp";

/// Stores a login with `access_token`, and no refresh token, for the
/// OAuth test server as "docs".
fn store_login(
    credentials: &std::path::Path,
    server: &crate::oauth::test_server::OAuthTestServer,
    access_token: &str,
) {
    use orca_core::config::mcp_credentials::{McpCredential, save_mcp_credential};

    save_mcp_credential(
        credentials,
        "docs",
        &McpCredential {
            server_url: server.mcp_url(),
            access_token: access_token.to_string(),
            refresh_token: None,
            expires_at: None,
            token_endpoint: format!("{}/token", server.url()),
            client_id: "configured-client".to_string(),
            resource: server.mcp_url(),
            scope: None,
        },
    )
    .expect("store a login");
}

/// The OAuth test server as "docs", connected with the stored login
/// `at-stored`, which the server must take, and the directory that
/// holds the login, in `mcp-credentials.json`.
fn logged_in_registry(
    server: &crate::oauth::test_server::OAuthTestServer,
) -> (McpRegistry, tempfile::TempDir) {
    let home = tempfile::tempdir().expect("temp dir");
    let credentials = home.path().join("mcp-credentials.json");
    store_login(&credentials, server, "at-stored");
    let registry = connected_registry(&[server.config("docs")], Some(credentials));
    assert_eq!(
        registry.server_statuses(),
        [status("docs", McpServerState::Ready)]
    );
    (registry, home)
}

/// Counts the changes `registry` tells a subscriber of, for as long as
/// the subscription lasts.
fn count_changes(registry: &McpRegistry) -> (McpChangeSubscription, Arc<AtomicUsize>) {
    let changes = Arc::new(AtomicUsize::new(0));
    let subscription = registry.subscribe(Arc::new({
        let changes = Arc::clone(&changes);
        move |_: &McpRegistry| {
            changes.fetch_add(1, Ordering::SeqCst);
        }
    }));
    (subscription, changes)
}

/// A client of the server `config` describes, talking through
/// `transport`.
fn client_with(config: McpServerConfig, transport: impl McpTransport + 'static) -> Arc<McpClient> {
    Arc::new(McpClient {
        server_name: canonical_mcp_name(&config.name),
        config,
        credentials_path: None,
        capabilities: McpServerCapabilities::default(),
        reconnect_failure: Mutex::default(),
        stop: Arc::default(),
        transport: Mutex::new(Arc::new(transport)),
    })
}

#[test]
fn marking_needs_login_notifies_subscribers() {
    use crate::oauth::test_server::{OAuthTestBehavior, OAuthTestServer};

    let server = OAuthTestServer::start(OAuthTestBehavior {
        accepted_tokens: vec!["at-stored".to_string()],
        ..Default::default()
    });
    let (registry, _home) = logged_in_registry(&server);
    let (_subscription, changes) = count_changes(&registry);
    let echo = registry
        .resolve_tool("mcp__docs__echo")
        .expect("the echo tool");
    server.revoke_tokens();

    registry
        .call_tool(&echo, serde_json::json!({}))
        .expect_err("the server turns the token away");

    assert_eq!(
        registry.server_statuses(),
        [status("docs", McpServerState::NeedsLogin)]
    );
    assert_eq!(changes.load(Ordering::SeqCst), 1);
    // Marked already: another refusal changes nothing, and tells no one.
    registry
        .call_tool(&echo, serde_json::json!({}))
        .expect_err("the server still turns the token away");
    assert_eq!(changes.load(Ordering::SeqCst), 1);
}

#[test]
fn a_failure_on_a_replaced_connection_does_not_mark_the_new_one() {
    use crate::oauth::test_server::{OAuthTestBehavior, OAuthTestServer};

    /// A connection whose tool calls run `during_call`, and then find
    /// the login gone.
    struct LoginGoneDuringCall {
        during_call: Box<dyn Fn() + Send + Sync>,
    }

    impl McpTransport for LoginGoneDuringCall {
        fn initialize(&self) -> Result<Value, String> {
            Ok(serde_json::json!({}))
        }

        fn list_tools(&self, _cursor: Option<&str>) -> Result<Value, String> {
            Ok(serde_json::json!({"tools": []}))
        }

        fn call_tool(&self, _name: &str, _arguments: Value) -> Result<Value, String> {
            (self.during_call)();
            Err(LOGIN_REQUIRED.to_string())
        }

        fn list_resources(&self, _cursor: Option<&str>) -> Result<Value, String> {
            Err("not asked for".to_string())
        }

        fn list_resource_templates(&self, _cursor: Option<&str>) -> Result<Value, String> {
            Err("not asked for".to_string())
        }

        fn read_resource(&self, _uri: &str) -> Result<Value, String> {
            Err("not asked for".to_string())
        }
    }

    // The server takes the configured token, so each connection to it
    // works.
    let server = OAuthTestServer::start(OAuthTestBehavior {
        accepted_tokens: vec!["at-configured".to_string()],
        ..Default::default()
    });
    let mut config = server.config("docs");
    config.headers.insert(
        "Authorization".to_string(),
        "Bearer at-configured".to_string(),
    );
    let registry = connected_registry(std::slice::from_ref(&config), None);
    assert_eq!(
        registry.server_statuses(),
        [status("docs", McpServerState::Ready)]
    );
    // A call is under way on the server's connection when the server is
    // reconnected, and then finds the login gone.
    let reconnect = {
        let registry = Arc::downgrade(&registry.shared);
        move || {
            let shared = registry.upgrade().expect("the registry");
            McpRegistry { shared }
                .reconnect_server("docs")
                .expect("reconnect the server");
        }
    };
    let old = client_with(
        config,
        LoginGoneDuringCall {
            during_call: Box::new(reconnect),
        },
    );
    let replaced = registry.write().clients.insert("docs".to_string(), old);
    drop(replaced);
    let echo = registry
        .resolve_tool("mcp__docs__echo")
        .expect("the echo tool");

    let error = registry
        .call_tool(&echo, serde_json::json!({}))
        .expect_err("the old connection finds the login gone");

    assert_eq!(error, LOGIN_REQUIRED);
    assert_eq!(
        registry.server_statuses(),
        [status("docs", McpServerState::Ready)],
        "the new connection's login works"
    );
    registry
        .call_tool(&echo, serde_json::json!({}))
        .expect("the new connection serves the next call");
}

#[test]
fn a_list_cut_short_on_a_replaced_connection_is_not_noted_on_the_new_one() {
    use crate::oauth::test_server::{OAuthTestBehavior, OAuthTestServer};

    /// A connection that names the same cursor on every page, and runs
    /// `during_second_page` while it reads the second.
    struct RepeatingCursor {
        during_second_page: Box<dyn Fn() + Send + Sync>,
    }

    impl McpTransport for RepeatingCursor {
        fn initialize(&self) -> Result<Value, String> {
            Ok(serde_json::json!({}))
        }

        fn list_tools(&self, _cursor: Option<&str>) -> Result<Value, String> {
            Ok(serde_json::json!({"tools": []}))
        }

        fn call_tool(&self, _name: &str, _arguments: Value) -> Result<Value, String> {
            Err("not asked for".to_string())
        }

        fn list_resources(&self, cursor: Option<&str>) -> Result<Value, String> {
            if cursor.is_some() {
                (self.during_second_page)();
            }
            Ok(serde_json::json!({
                "resources": [{"uri": "memo://1", "name": "one"}],
                "nextCursor": "c1"
            }))
        }

        fn list_resource_templates(&self, _cursor: Option<&str>) -> Result<Value, String> {
            Err("not asked for".to_string())
        }

        fn read_resource(&self, _uri: &str) -> Result<Value, String> {
            Err("not asked for".to_string())
        }
    }

    // The server takes the configured token, so each connection to it
    // works.
    let server = OAuthTestServer::start(OAuthTestBehavior {
        accepted_tokens: vec!["at-configured".to_string()],
        ..Default::default()
    });
    let mut config = server.config("docs");
    config.headers.insert(
        "Authorization".to_string(),
        "Bearer at-configured".to_string(),
    );
    let registry = connected_registry(std::slice::from_ref(&config), None);
    // A listing is under way on the server's connection when the server
    // is reconnected, and then finds the list cut short.
    let reconnect = {
        let registry = Arc::downgrade(&registry.shared);
        move || {
            let shared = registry.upgrade().expect("the registry");
            McpRegistry { shared }
                .reconnect_server("docs")
                .expect("reconnect the server");
        }
    };
    let old = client_with(
        config,
        RepeatingCursor {
            during_second_page: Box::new(reconnect),
        },
    );
    let replaced = registry.write().clients.insert("docs".to_string(), old);
    drop(replaced);
    let (_subscription, changes) = count_changes(&registry);

    let resources = registry
        .list_resources(Some("docs"))
        .expect("the resources read");

    assert_eq!(resources.len(), 2);
    assert_eq!(
        registry.server_statuses(),
        [status("docs", McpServerState::Ready)],
        "the new connection's list is not the one cut short"
    );
    // The reconnect, and nothing else.
    assert_eq!(changes.load(Ordering::SeqCst), 1);
}

#[test]
fn a_prompt_or_resource_request_that_needs_login_marks_the_server() {
    use crate::oauth::test_server::{OAuthTestBehavior, OAuthTestServer};

    for request in [
        "prompts/get",
        "resources/list",
        "resources/templates/list",
        "resources/read",
        "resources/list with errors",
        "resources/templates/list with errors",
    ] {
        let server = OAuthTestServer::start(OAuthTestBehavior {
            accepted_tokens: vec!["at-stored".to_string()],
            offers_prompts_and_resources: true,
            ..Default::default()
        });
        let (registry, _home) = logged_in_registry(&server);
        server.revoke_tokens();

        let error = match request {
            "prompts/get" => registry
                .get_prompt("docs", "review", &BTreeMap::new())
                .map(drop),
            "resources/list" => registry.list_resources(Some("docs")).map(drop),
            "resources/templates/list" => registry.list_resource_templates(Some("docs")).map(drop),
            "resources/read" => registry.read_resource("docs", "memo://readme").map(drop),
            // These list what they can, with an error for each server
            // that failed.
            "resources/list with errors" => Err(registry
                .list_resources_with_errors(Some("docs"))
                .errors
                .join("\n")),
            _ => Err(registry
                .list_resource_templates_with_errors(Some("docs"))
                .errors
                .join("\n")),
        }
        .expect_err("the server turns the token away");

        let expected = if request.ends_with("with errors") {
            format!("docs: {LOGIN_REQUIRED}")
        } else {
            LOGIN_REQUIRED.to_string()
        };
        assert_eq!(error, expected, "{request}");
        assert_eq!(
            registry.server_statuses(),
            [status("docs", McpServerState::NeedsLogin)],
            "{request}"
        );
    }
}

#[test]
fn a_successful_request_clears_needs_login() {
    use crate::oauth::test_server::{OAuthTestBehavior, OAuthTestServer};

    let server = OAuthTestServer::start(OAuthTestBehavior {
        accepted_tokens: vec!["at-stored".to_string()],
        ..Default::default()
    });
    let (registry, home) = logged_in_registry(&server);
    let (_subscription, changes) = count_changes(&registry);
    let echo = registry
        .resolve_tool("mcp__docs__echo")
        .expect("the echo tool");
    server.revoke_tokens();
    let error = registry
        .call_tool(&echo, serde_json::json!({}))
        .expect_err("the server turns the token away");
    assert_eq!(error, LOGIN_REQUIRED);
    assert_eq!(
        registry.server_statuses(),
        [status("docs", McpServerState::NeedsLogin)]
    );
    assert_eq!(changes.load(Ordering::SeqCst), 1);

    // The user logs in again, from another Orca: the connection picks
    // the new login up on its next request.
    server.accept_token("at-again");
    store_login(
        &home.path().join("mcp-credentials.json"),
        &server,
        "at-again",
    );
    registry
        .call_tool(&echo, serde_json::json!({}))
        .expect("the same connection goes through with the new login");

    assert_eq!(
        registry.server_statuses(),
        [status("docs", McpServerState::Ready)]
    );
    assert_eq!(changes.load(Ordering::SeqCst), 2);
    // Ready already: another success changes nothing, and tells no one.
    registry
        .call_tool(&echo, serde_json::json!({}))
        .expect("the next call goes through too");
    assert_eq!(changes.load(Ordering::SeqCst), 2);
    // All of it went through the first connection.
    let initializations = server
        .requests_to("/mcp")
        .iter()
        .filter(|request| request.json()["method"] == "initialize")
        .count();
    assert_eq!(initializations, 1);
}

#[test]
fn disabled_servers_are_listed() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let mut disabled = missing_server_config("off", temp_dir.path());
    disabled.disabled = true;
    let registry = connected_registry(
        &[disabled, missing_server_config("broken", temp_dir.path())],
        None,
    );

    let states = registry.server_statuses();
    assert_eq!(states.len(), 2, "{states:?}");
    assert_eq!(states[0], status("off", McpServerState::Disabled));
    assert_eq!(states[1].name, "broken");
    assert!(
        matches!(states[1].state, McpServerState::Failed { .. }),
        "{states:?}"
    );
    assert_eq!(
        registry.errors().len(),
        1,
        "a disabled server is never started: {:?}",
        registry.errors()
    );

    assert_eq!(
        registry.reconnect_server("off"),
        Err("MCP server 'off' is disabled".to_string())
    );
    assert_eq!(
        registry.server_statuses()[0],
        status("off", McpServerState::Disabled)
    );
    assert_eq!(
        registry.reconnect_server("nope"),
        Err("no MCP server named 'nope'".to_string())
    );
}

#[cfg(unix)]
#[test]
fn startup_returns_before_servers_connect() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let config = listing_server_config("slow", temp_dir.path(), "[]");
    delay_starts(temp_dir.path(), "slow", 1);

    let started = Instant::now();
    let registry = initialize_registry(&[config], None);

    let elapsed = started.elapsed();
    assert!(elapsed < Duration::from_millis(300), "{elapsed:?}");
    assert_eq!(
        registry.server_statuses(),
        [status("slow", McpServerState::Starting)]
    );
    assert!(registry.is_starting());
    assert!(registry.wait_for_startup(&|| false));
    assert_eq!(
        registry.server_statuses(),
        [status("slow", McpServerState::Ready)]
    );
    assert!(!registry.is_starting());
}

#[cfg(unix)]
#[test]
fn servers_connect_in_parallel() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let configs = ["one", "two"].map(|name| {
        let config = listing_server_config(
            name,
            temp_dir.path(),
            r#"[{"name":"echo","inputSchema":{"type":"object"}}]"#,
        );
        delay_starts(temp_dir.path(), name, 1);
        config
    });

    let started = Instant::now();
    let registry = initialize_registry(&configs, None);
    let finished = registry.wait_for_startup(&|| false);

    let elapsed = started.elapsed();
    assert!(finished);
    assert!(elapsed < Duration::from_millis(1_800), "{elapsed:?}");
    assert_eq!(
        registry.server_statuses(),
        [
            status("one", McpServerState::Ready),
            status("two", McpServerState::Ready),
        ]
    );
    assert_eq!(
        schema_names(&registry),
        ["mcp__one__echo", "mcp__two__echo"]
    );
}

#[cfg(unix)]
#[test]
fn a_subscriber_hears_each_server_finish() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let configs = ["one", "two"].map(|name| {
        let config = listing_server_config(name, temp_dir.path(), "[]");
        delay_starts(temp_dir.path(), name, 1);
        config
    });
    let registry = initialize_registry(&configs, None);
    let heard = Arc::new(Mutex::new(Vec::new()));
    let _subscription = registry.subscribe(Arc::new({
        let heard = Arc::clone(&heard);
        move |registry: &McpRegistry| {
            // `heard` is locked before the registry is read, so that two
            // calls made at once, by the two servers' threads, push their
            // reads in the order they made them: each read finds at least
            // the servers finished that the one before it found.
            heard
                .lock()
                .expect("the calls heard")
                .push(registry.server_statuses());
        }
    }));

    assert!(registry.wait_for_startup(&|| false));
    wait_for("both servers to be heard", || {
        heard.lock().expect("the calls heard").len() >= 2
    });

    let heard = heard.lock().expect("the calls heard");
    // Each call reads the registry with the server it is about already
    // finished.
    for (index, statuses) in heard.iter().enumerate() {
        let finished = statuses
            .iter()
            .filter(|status| status.state != McpServerState::Starting)
            .count();
        assert!(finished > index.min(1), "call {index} read {statuses:?}");
    }
    assert_eq!(
        heard.last().expect("a call"),
        &[
            status("one", McpServerState::Ready),
            status("two", McpServerState::Ready),
        ]
    );
}

#[test]
fn dropping_a_subscription_stops_its_calls() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let registry = connected_registry(&[missing_server_config("broken", temp_dir.path())], None);
    let (subscription, changes) = count_changes(&registry);
    registry
        .reconnect_server("broken")
        .expect_err("its command is still missing");
    // It was starting again, and then failed again.
    assert_eq!(changes.load(Ordering::SeqCst), 2);

    drop(subscription);
    registry
        .reconnect_server("broken")
        .expect_err("its command is still missing");

    assert_eq!(
        changes.load(Ordering::SeqCst),
        2,
        "a dropped subscription is called no more"
    );
}

#[cfg(unix)]
#[test]
fn waiting_for_startup_can_be_cancelled() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let config = listing_server_config("slow", temp_dir.path(), "[]");
    delay_starts(temp_dir.path(), "slow", 5);
    let registry = initialize_registry(&[config], None);
    let cancel = Arc::new(AtomicBool::new(false));
    let canceller = std::thread::spawn({
        let cancel = Arc::clone(&cancel);
        move || {
            std::thread::sleep(Duration::from_millis(300));
            let cancelled = Instant::now();
            cancel.store(true, Ordering::SeqCst);
            cancelled
        }
    });

    let finished = registry.wait_for_startup(&|| cancel.load(Ordering::SeqCst));

    let returned = Instant::now();
    let cancelled = canceller.join().expect("the canceller");
    assert!(!finished);
    let late = returned.duration_since(cancelled);
    assert!(late < Duration::from_millis(200), "{late:?}");
    assert_eq!(
        registry.server_statuses(),
        [status("slow", McpServerState::Starting)]
    );
}

#[cfg(unix)]
#[test]
fn a_reconnect_started_while_starting_wins() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let config = listing_server_config(
        "x",
        temp_dir.path(),
        r#"[{"name":"first","inputSchema":{"type":"object"}}]"#,
    );
    delay_starts(temp_dir.path(), "x", 1);
    let registry = initialize_registry(&[config], None);
    // Once its pid is in, the first start has read its tools and its
    // delay; the next start lists `second`, at once.
    wait_for("the first start", || {
        start_pids(temp_dir.path(), "x").len() == 1
    });
    let state = temp_dir.path().join("x");
    std::fs::write(
        state.join("tools.json"),
        r#"[{"name":"second","inputSchema":{"type":"object"}}]"#,
    )
    .expect("change the tool list");
    std::fs::remove_file(state.join("delay")).expect("drop the delay");

    registry
        .reconnect_server("x")
        .expect("reconnect the server");

    // The reconnect stopped the first start before it started the server
    // again; the first start's connection, overtaken, is dropped.
    let first = start_pids(temp_dir.path(), "x").remove(0);
    wait_for("the first start to stop", || !process_is_alive(&first));
    assert_eq!(
        overlapping_starts(temp_dir.path(), "x"),
        Vec::<String>::new()
    );
    assert_eq!(schema_names(&registry), ["mcp__x__second"]);
    assert_eq!(
        registry.server_statuses(),
        [status("x", McpServerState::Ready)]
    );
    assert_eq!(starts(temp_dir.path(), "x"), 2);
}

/// A stdio server that adds its pid to `<dir>/<name>/starts` as it
/// starts, and then reads what it is sent without ever answering, so
/// that it is connecting until it is stopped, or until its startup
/// timeout.
#[cfg(unix)]
fn silent_server_config(name: &str, dir: &std::path::Path) -> McpServerConfig {
    let state = dir.join(name);
    std::fs::create_dir_all(&state).expect("state dir");
    let script = dir.join(format!("{name}.sh"));
    std::fs::write(
        &script,
        "#!/bin/sh\nprintf '%s\\n' \"$$\" >> \"$1/starts\"\nwhile IFS= read -r line; do :; done\n",
    )
    .expect("write MCP fixture");
    let mut config = stdio_fixture_config(name, &script);
    config.args.push(state.to_string_lossy().into_owned());
    config
}

/// Fails the test, saying `what`, unless the process `pid` is gone
/// within half a second.
#[cfg(unix)]
fn assert_gone_soon(pid: &str, what: &str) {
    let deadline = Instant::now() + Duration::from_millis(500);
    while process_is_alive(pid) {
        assert!(
            Instant::now() < deadline,
            "{what}: process {pid} still runs"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
#[test]
fn dropping_the_registry_stops_a_server_still_connecting() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let registry = initialize_registry(&[silent_server_config("silent", temp_dir.path())], None);
    wait_for("the server to start", || {
        starts(temp_dir.path(), "silent") == 1
    });
    let pid = start_pids(temp_dir.path(), "silent").remove(0);
    assert_eq!(
        registry.server_statuses(),
        [status("silent", McpServerState::Starting)]
    );

    drop(registry);

    // Its connection would have waited for `initialize` for its whole
    // startup timeout, on a thread a process that exits does not wait
    // for.
    assert_gone_soon(&pid, "the server outlived the registry");
}

#[cfg(unix)]
#[test]
fn dropping_the_registry_keeps_no_strong_reference_in_connect_threads() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let fast = listing_server_config("fast", temp_dir.path(), "[]");
    let slow = silent_server_config("slow", temp_dir.path());
    let registry = initialize_registry(&[fast, slow], None);
    wait_for("the fast server to connect", || {
        registry.server_statuses()[0].state == McpServerState::Ready
    });
    wait_for("the slow server to start", || {
        starts(temp_dir.path(), "slow") == 1
    });
    let fast = start_pids(temp_dir.path(), "fast").remove(0);
    let slow = start_pids(temp_dir.path(), "slow").remove(0);
    assert!(registry.is_starting());

    drop(registry);

    // Had the slow server's thread, still connecting, held the registry,
    // this would not have been the last holder, and neither server would
    // have stopped.
    assert_gone_soon(&fast, "the fast server outlived the registry");
    assert_gone_soon(&slow, "the slow server outlived the registry");
}

#[cfg(unix)]
#[test]
fn closing_the_registry_stops_its_servers_whoever_still_holds_it() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let echo = r#"[{"name":"echo","inputSchema":{"type":"object"}}]"#;
    let registry = initialize_registry(
        &[
            listing_server_config("ready", temp_dir.path(), echo),
            silent_server_config("silent", temp_dir.path()),
        ],
        None,
    );
    wait_for("the ready server to connect", || {
        registry.server_statuses()[0].state == McpServerState::Ready
    });
    wait_for("the silent server to start", || {
        starts(temp_dir.path(), "silent") == 1
    });
    let ready = start_pids(temp_dir.path(), "ready").remove(0);
    let silent = start_pids(temp_dir.path(), "silent").remove(0);
    // Another holder, as a worker that the process does not wait for
    // when it exits.
    let holder = registry.clone();
    let (_subscription, changes) = count_changes(&registry);

    registry.close();

    assert_gone_soon(&ready, "the connected server outlived the close");
    assert_gone_soon(&silent, "the server still connecting outlived the close");
    let stopped = |name: &str| {
        status(
            name,
            McpServerState::Failed {
                message: format!("MCP server '{name}' was stopped"),
            },
        )
    };
    assert_eq!(
        holder.server_statuses(),
        [stopped("ready"), stopped("silent")]
    );
    assert!(holder.wait_for_startup(&|| true), "a server is starting");
    assert!(holder.tools().is_empty());
    assert_eq!(
        holder.reconnect_server("ready"),
        Err("MCP server 'ready' was stopped".to_string())
    );
    assert_eq!(
        starts(temp_dir.path(), "ready"),
        1,
        "a server started again"
    );
    assert_eq!(changes.load(Ordering::SeqCst), 0, "a subscriber was told");

    // A connection that is made all the same, as one to a remote server
    // can be, is dropped.
    struct EchoTransport;

    impl McpTransport for EchoTransport {
        fn initialize(&self) -> Result<Value, String> {
            Ok(serde_json::json!({"capabilities": {}}))
        }

        fn list_tools(&self, _cursor: Option<&str>) -> Result<Value, String> {
            Ok(serde_json::json!({"tools": [
                {"name": "echo", "inputSchema": {"type": "object"}}
            ]}))
        }

        fn call_tool(&self, _name: &str, _arguments: Value) -> Result<Value, String> {
            Err("not used".to_string())
        }

        fn list_resources(&self, _cursor: Option<&str>) -> Result<Value, String> {
            Err("not used".to_string())
        }

        fn list_resource_templates(&self, _cursor: Option<&str>) -> Result<Value, String> {
            Err("not used".to_string())
        }

        fn read_resource(&self, _uri: &str) -> Result<Value, String> {
            Err("not used".to_string())
        }
    }
    let late = connect_server_with_transport(
        &McpServerConfig {
            name: "ready".to_string(),
            ..Default::default()
        },
        "ready",
        None,
        Arc::new(EchoTransport),
        Arc::default(),
    );
    assert!(late.is_ok());
    let _ = holder.finish_connecting("ready", FIRST_CONNECTION, late);
    assert_eq!(holder.server_statuses()[0], stopped("ready"));
    assert!(holder.tools().is_empty());
    assert!(holder.client("ready").is_none());
}

#[cfg(unix)]
#[test]
fn reconnecting_a_stdio_server_stops_the_old_process_first() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let config = listing_server_config(
        "x",
        temp_dir.path(),
        r#"[{"name":"echo","inputSchema":{"type":"object"}}]"#,
    );
    let registry = connected_registry(&[config], None);

    registry
        .reconnect_server("x")
        .expect("reconnect through the registry");
    let after_the_registry = overlapping_starts(temp_dir.path(), "x");
    let client = registry.client("x").expect("the server's client");
    let current = client.lock_transport().expect("its transport").clone();
    client
        .reconnect(&current, None, &|| false)
        .expect("reconnect through the client");
    let after_the_client = overlapping_starts(temp_dir.path(), "x");

    // Each start found the one before it stopped, so a server that
    // listens on a fixed port could start again.
    assert_eq!(starts(temp_dir.path(), "x"), 3);
    assert!(
        after_the_registry.is_empty() && after_the_client.is_empty(),
        "starts that found the one before still running: {after_the_registry:?} through the registry, then {after_the_client:?} through the client"
    );
    assert_eq!(
        registry.server_statuses(),
        [status("x", McpServerState::Ready)]
    );
    assert_eq!(schema_names(&registry), ["mcp__x__echo"]);
}

#[cfg(unix)]
#[test]
fn reconnecting_a_stdio_server_fails_a_call_still_under_way() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let mut config = listing_server_config(
        "x",
        temp_dir.path(),
        r#"[{"name":"wait","inputSchema":{"type":"object"}}]"#,
    );
    // The server never answers a call, which would wait this long.
    config.tool_timeout_ms = Some(STDIO_TEST_STARTUP_TIMEOUT_MS);
    let registry = connected_registry(&[config], None);
    let wait = registry
        .resolve_tool("mcp__x__wait")
        .expect("the wait tool");
    let call = std::thread::spawn({
        let registry = registry.clone();
        move || registry.call_tool(&wait, serde_json::json!({}))
    });
    let state = temp_dir.path().join("x");
    wait_for("the call to reach the server", || {
        logged_methods(&state)
            .iter()
            .any(|method| method == "tools/call")
    });
    let reconnecting = Instant::now();

    registry
        .reconnect_server("x")
        .expect("reconnect the server");

    let error = call
        .join()
        .expect("the call's thread")
        .expect_err("the call's server was stopped");
    let waited = reconnecting.elapsed();
    assert_eq!(error, "MCP server closed stdout");
    assert!(
        waited < Duration::from_secs(5),
        "the call went on {waited:?} after the reconnect began"
    );
    // Its server stopped before the new one started, and its connection,
    // stopped, started no other.
    assert_eq!(
        overlapping_starts(temp_dir.path(), "x"),
        Vec::<String>::new()
    );
    assert_eq!(starts(temp_dir.path(), "x"), 2);
    assert_eq!(
        registry.server_statuses(),
        [status("x", McpServerState::Ready)]
    );
}

#[cfg(unix)]
#[test]
fn a_stdio_server_is_starting_while_it_reconnects() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let config = listing_server_config(
        "slow",
        temp_dir.path(),
        r#"[{"name":"echo","inputSchema":{"type":"object"}}]"#,
    );
    let registry = connected_registry(&[config], None);
    let heard = Arc::new(Mutex::new(Vec::new()));
    let _subscription = registry.subscribe(Arc::new({
        let heard = Arc::clone(&heard);
        move |registry: &McpRegistry| {
            heard
                .lock()
                .expect("the states heard")
                .push(registry.server_statuses()[0].state.clone());
        }
    }));
    delay_starts(temp_dir.path(), "slow", 1);

    let reconnect = std::thread::spawn({
        let registry = registry.clone();
        move || registry.reconnect_server("slow")
    });

    // Stopped, and not yet started again, it has no client, so a turn
    // waits for it, as for a server still making its first connection.
    wait_for("the reconnect to begin", || registry.is_starting());
    assert_eq!(
        registry.server_statuses(),
        [status("slow", McpServerState::Starting)]
    );
    assert!(registry.wait_for_startup(&|| false));
    assert_eq!(
        registry.server_statuses(),
        [status("slow", McpServerState::Ready)]
    );
    assert_eq!(schema_names(&registry), ["mcp__slow__echo"]);
    reconnect
        .join()
        .expect("the reconnect's thread")
        .expect("reconnect the server");
    assert_eq!(
        *heard.lock().expect("the states heard"),
        [McpServerState::Starting, McpServerState::Ready]
    );
    assert_eq!(starts(temp_dir.path(), "slow"), 2);
}

#[test]
fn a_server_that_needs_login_ends_startup_as_needs_login() {
    use crate::oauth::test_server::{OAuthTestBehavior, OAuthTestServer};

    // It answers `initialize` with a 401, and no login is stored.
    let server = OAuthTestServer::start(OAuthTestBehavior::default());
    let registry = initialize_registry(&[server.config("docs")], None);

    assert!(registry.wait_for_startup(&|| false));
    assert_eq!(
        registry.server_statuses(),
        [McpServerStatus {
            errors: vec![LOGIN_REQUIRED.to_string()],
            ..status("docs", McpServerState::NeedsLogin)
        }]
    );
    assert!(registry.tools().is_empty());
}

#[cfg(unix)]
#[test]
fn a_prompt_list_failure_is_kept_on_the_server_status() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let config = broken_prompt_list_server(
        temp_dir.path(),
        Some(r#"{"jsonrpc":"2.0","id":%s,"error":{"code":-32601,"message":"Method not found"}}"#),
    );
    let registry = initialize_registry(&[config], None);

    assert!(registry.wait_for_startup(&|| false));
    assert_eq!(
            registry.server_statuses(),
            [McpServerStatus {
                name: "broken".to_string(),
                state: McpServerState::Ready,
                prompts_error: Some(
                    r#"MCP request 'prompts/list' failed: {"code":-32601,"message":"Method not found"}"#
                        .to_string()
                ),
                errors: Vec::new(),
            }]
        );
    assert!(registry.errors().is_empty(), "{:?}", registry.errors());
    assert!(registry.resolve_tool("mcp__broken__echo").is_some());
}

/// A stdio server that offers prompts. It declares the capabilities in
/// `<dir>/<name>/capabilities.json`, lists the prompts in
/// `<dir>/<name>/prompts.json`, and answers each `prompts/get` with the
/// result in `<dir>/<name>/get.json`, or with the error in
/// `<dir>/<name>/get-error.json` once that exists. Each message it gets
/// is added to `<dir>/<name>/requests`.
#[cfg(unix)]
fn prompt_server_config(
    name: &str,
    dir: &std::path::Path,
    capabilities: &str,
    prompts: &str,
    get_result: &str,
) -> McpServerConfig {
    let state = dir.join(name);
    std::fs::create_dir_all(&state).expect("state dir");
    std::fs::write(state.join("capabilities.json"), capabilities).expect("write the capabilities");
    std::fs::write(state.join("prompts.json"), prompts).expect("write the prompt list");
    std::fs::write(state.join("get.json"), get_result).expect("write the prompt");
    let script = dir.join(format!("{name}.sh"));
    std::fs::write(
            &script,
            r#"#!/bin/sh
state_dir="$1"
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$state_dir/requests"
  id=${line#*'"id":'}
  id=${id%%,*}
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2024-11-05","capabilities":%s,"serverInfo":{"name":"prompts","version":"1"}}}\n' "$id" "$(cat "$state_dir/capabilities.json")"
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[]}}\n' "$id"
      ;;
    *'"method":"prompts/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"prompts":%s}}\n' "$id" "$(cat "$state_dir/prompts.json")"
      ;;
    *'"method":"prompts/get"'*)
      if [ -f "$state_dir/get-error.json" ]; then
        printf '{"jsonrpc":"2.0","id":%s,"error":%s}\n' "$id" "$(cat "$state_dir/get-error.json")"
      else
        printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$id" "$(cat "$state_dir/get.json")"
      fi
      ;;
  esac
done
"#,
        )
        .expect("write MCP fixture");
    let mut config = stdio_fixture_config(name, &script);
    config.args.push(state.to_string_lossy().into_owned());
    config.tool_timeout_ms = Some(STDIO_TEST_STARTUP_TIMEOUT_MS);
    config
}

/// The messages with `method` that the prompt server `name` got.
#[cfg(unix)]
fn prompt_server_requests(dir: &std::path::Path, name: &str, method: &str) -> Vec<Value> {
    std::fs::read_to_string(dir.join(name).join("requests"))
        .expect("read the request log")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("a JSON-RPC message"))
        .filter(|message| message["method"] == method)
        .collect()
}

#[cfg(unix)]
fn prompt_names(registry: &McpRegistry) -> Vec<String> {
    registry
        .prompts()
        .into_iter()
        .map(|prompt| prompt.name)
        .collect()
}

#[cfg(unix)]
#[test]
fn prompts_are_listed_for_servers_that_offer_them() {
    use orca_core::mcp_types::McpPromptArgument;

    let temp_dir = tempfile::tempdir().expect("temp dir");
    // Its prompts are listed under "docs", the name in its tools' names.
    let offering = prompt_server_config(
        "Docs",
        temp_dir.path(),
        r#"{"prompts":{}}"#,
        r#"[{"name":"review_pr","description":"Reviews a pull request","arguments":[{"name":"pr","description":"The pull request","required":true},{"name":"branch"}]},{"name":"summarize"}]"#,
        r#"{"messages":[]}"#,
    );
    // It would list a prompt, but it does not declare the capability.
    let silent = prompt_server_config(
        "plain",
        temp_dir.path(),
        "{}",
        r#"[{"name":"hidden"}]"#,
        r#"{"messages":[]}"#,
    );

    let registry = connected_registry(&[offering, silent], None);

    assert!(registry.errors().is_empty(), "{:?}", registry.errors());
    assert_eq!(
        registry.prompts(),
        [
            McpPrompt {
                server: "docs".to_string(),
                name: "review_pr".to_string(),
                description: Some("Reviews a pull request".to_string()),
                arguments: vec![
                    McpPromptArgument {
                        name: "pr".to_string(),
                        description: Some("The pull request".to_string()),
                        required: true,
                    },
                    McpPromptArgument {
                        name: "branch".to_string(),
                        description: None,
                        required: false,
                    },
                ],
            },
            McpPrompt {
                server: "docs".to_string(),
                name: "summarize".to_string(),
                description: None,
                arguments: Vec::new(),
            },
        ]
    );
    assert!(
        prompt_server_requests(temp_dir.path(), "plain", "prompts/list").is_empty(),
        "a server that does not offer prompts is not asked for them"
    );
}

#[cfg(unix)]
#[test]
fn a_prompt_expands_to_text_and_images() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let prompt = serde_json::json!({
        "description": "Reviews a pull request",
        "messages": [
            {
                "role": "user",
                "content": {"type": "text", "text": "Review pull request 123 against main."}
            },
            {
                "role": "user",
                "content": {"type": "image", "data": BASE64_1X1_PNG, "mimeType": "image/png"}
            }
        ]
    });
    let config = prompt_server_config(
        "docs",
        temp_dir.path(),
        r#"{"prompts":{}}"#,
        r#"[{"name":"review_pr","arguments":[{"name":"pr","required":true},{"name":"branch"}]}]"#,
        &prompt.to_string(),
    );
    let registry = connected_registry(&[config], None);
    assert!(registry.errors().is_empty(), "{:?}", registry.errors());

    let expansion = registry
        .get_prompt(
            "docs",
            "review_pr",
            &BTreeMap::from([
                ("pr".to_string(), "123".to_string()),
                ("branch".to_string(), "main".to_string()),
            ]),
        )
        .expect("expand the prompt");

    assert_eq!(
        expansion,
        McpPromptExpansion {
            text: "Review pull request 123 against main.".to_string(),
            images: vec![tool_image("image/png", BASE64_1X1_PNG.to_string()).expect("a valid PNG")],
        }
    );
    let asked = prompt_server_requests(temp_dir.path(), "docs", "prompts/get");
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert_eq!(
        asked[0]["params"],
        serde_json::json!({"name": "review_pr", "arguments": {"pr": "123", "branch": "main"}})
    );
}

#[cfg(unix)]
#[test]
fn an_unknown_prompt_is_an_error() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let registry = connected_registry(
        &[
            prompt_server_config(
                "docs",
                temp_dir.path(),
                r#"{"prompts":{}}"#,
                r#"[{"name":"review_pr"}]"#,
                r#"{"messages":[]}"#,
            ),
            missing_server_config("broken", temp_dir.path()),
        ],
        None,
    );
    let no_arguments = BTreeMap::new();

    assert_eq!(
        registry.get_prompt("docs", "deploy", &no_arguments),
        Err("MCP server 'docs' has no prompt named 'deploy'".to_string())
    );
    assert_eq!(
        registry.get_prompt("wiki", "review_pr", &no_arguments),
        Err("no MCP server named 'wiki'".to_string())
    );
    assert_eq!(
        registry.get_prompt("broken", "review_pr", &no_arguments),
        Err("MCP server 'broken' is not connected".to_string())
    );
    assert!(
        prompt_server_requests(temp_dir.path(), "docs", "prompts/get").is_empty(),
        "a prompt the server does not list is not asked for"
    );

    // A prompt the server listed, then no longer knows.
    std::fs::write(
        temp_dir.path().join("docs").join("get-error.json"),
        r#"{"code":-32602,"message":"Unknown prompt: review_pr"}"#,
    )
    .expect("make the server refuse the prompt");
    let error = registry
        .get_prompt("docs", "review_pr", &no_arguments)
        .expect_err("the server refuses the prompt");
    assert!(error.contains("Unknown prompt: review_pr"), "{error}");
}

#[cfg(unix)]
#[test]
fn reconnecting_a_server_refreshes_its_prompts() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let registry = connected_registry(
        &[prompt_server_config(
            "docs",
            temp_dir.path(),
            r#"{"prompts":{}}"#,
            r#"[{"name":"before"}]"#,
            r#"{"messages":[]}"#,
        )],
        None,
    );
    assert_eq!(prompt_names(&registry), ["before"]);
    std::fs::write(
        temp_dir.path().join("docs").join("prompts.json"),
        r#"[{"name":"after"}]"#,
    )
    .expect("change the prompt list");

    registry
        .reconnect_server("docs")
        .expect("reconnect the server");
    assert_eq!(prompt_names(&registry), ["after"]);

    // A server that cannot be reached offers no prompts.
    std::fs::remove_file(temp_dir.path().join("docs.sh")).expect("remove the server");
    registry
        .reconnect_server("docs")
        .expect_err("the server is gone");
    assert!(registry.prompts().is_empty());
}

#[cfg(unix)]
#[test]
fn a_server_whose_prompt_list_fails_stays_connected() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let registry = connected_registry(
        &[prompt_server_config(
            "docs",
            temp_dir.path(),
            r#"{"prompts":{}}"#,
            r#""not a list""#,
            r#"{"messages":[]}"#,
        )],
        None,
    );

    let statuses = registry.server_statuses();
    assert_eq!(statuses.len(), 1, "{statuses:?}");
    assert_eq!(statuses[0].state, McpServerState::Ready);
    assert!(
        statuses[0]
            .prompts_error
            .as_deref()
            .is_some_and(|error| error.starts_with("invalid prompts/list result for 'docs'")),
        "{statuses:?}"
    );
    assert!(registry.prompts().is_empty());
    assert!(registry.errors().is_empty(), "{:?}", registry.errors());
}

#[test]
fn a_prompt_expansion_keeps_its_messages_in_order() {
    let result = serde_json::from_value::<GetPromptResult>(serde_json::json!({
            "messages": [
                {"role": "user", "content": {"type": "text", "text": "Intro"}},
                {"role": "assistant", "content": {"type": "image", "data": "PHN2Zz4=", "mimeType": "image/svg+xml"}},
                {"role": "user", "content": {"type": "resource", "resource": {"uri": "file:///notes.md", "mimeType": "text/markdown", "text": "Notes"}}},
                {"role": "user", "content": {"type": "resource", "resource": {"uri": "file:///logo.png", "blob": BASE64_1X1_PNG}}},
                {"role": "user", "content": {"type": "resource_link", "uri": "file:///elsewhere.md", "name": "elsewhere"}},
                {"role": "user", "content": {"type": "audio", "data": "AAAA", "mimeType": "audio/wav"}},
                {"role": "user", "content": {"type": "image", "data": BASE64_1X1_PNG, "mimeType": "image/png"}},
                {"role": "user", "content": {"type": "text", "text": ""}},
                {"role": "assistant", "content": {"type": "text", "text": "Outro"}}
            ]
        }))
        .expect("a prompts/get result");

    let expansion = expand_prompt(result);

    assert_eq!(
        expansion.text,
        "Intro\n\n[image omitted: unsupported type image/svg+xml]\n\nNotes\n\nOutro"
    );
    assert_eq!(
        expansion.images,
        [tool_image("image/png", BASE64_1X1_PNG.to_string()).expect("a valid PNG")]
    );
}

#[test]
fn prompts_without_a_name_are_left_out() {
    let mut errors = Vec::new();
    let listed: PromptsListResult = serde_json::from_value(serde_json::json!({"prompts": [
        {"name": "review_pr", "arguments": [{"name": "pr"}]},
        {"name": " "},
        {"name": "summarize", "arguments": [{"name": ""}]}
    ]}))
    .expect("a prompts/list result");

    let prompts = listed_prompts("docs", listed.prompts, &mut errors);

    assert_eq!(
        prompts
            .iter()
            .map(|prompt| prompt.name.as_str())
            .collect::<Vec<_>>(),
        ["review_pr"]
    );
    assert_eq!(
        errors,
        [
            "MCP prompt without a name, skipping from 'docs'",
            "MCP prompt 'summarize' has an argument without a name, skipping from 'docs'",
        ]
    );
}

/// A stdio server that declares prompts and serves the tool `echo`, but
/// answers `prompts/list` with `prompt_list_reply`, a JSON-RPC message
/// in which `%s` stands for the request id, or never when there is none.
/// Each message it gets is added to `<dir>/requests`.
#[cfg(unix)]
fn broken_prompt_list_server(
    dir: &std::path::Path,
    prompt_list_reply: Option<&str>,
) -> McpServerConfig {
    if let Some(reply) = prompt_list_reply {
        std::fs::write(dir.join("prompt-list-reply"), reply).expect("write the prompts/list reply");
    }
    let script = dir.join("broken_prompt_list.sh");
    std::fs::write(
            &script,
            r#"#!/bin/sh
state_dir="$1"
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$state_dir/requests"
  id=${line#*'"id":'}
  id=${id%%,*}
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2024-11-05","capabilities":{"prompts":{}},"serverInfo":{"name":"broken","version":"1"}}}\n' "$id"
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","inputSchema":{"type":"object"}}]}}\n' "$id"
      ;;
    *'"method":"prompts/list"'*)
      if [ -f "$state_dir/prompt-list-reply" ]; then
        printf "$(cat "$state_dir/prompt-list-reply")\n" "$id"
      fi
      ;;
    *'"method":"tools/call"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"echoed"}],"isError":false}}\n' "$id"
      ;;
  esac
done
"#,
        )
        .expect("write MCP fixture");
    let mut config = stdio_fixture_config("broken", &script);
    config.args.push(dir.to_string_lossy().into_owned());
    config.tool_timeout_ms = Some(STDIO_TEST_STARTUP_TIMEOUT_MS);
    config
}

/// The method of each message the server logged to `<dir>/requests`, in
/// the order they arrived.
#[cfg(unix)]
fn logged_methods(dir: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("requests"))
        .expect("read the request log")
        .lines()
        .map(|line| {
            serde_json::from_str::<Value>(line).expect("a JSON-RPC message")["method"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .collect()
}

#[cfg(unix)]
#[test]
fn a_server_that_never_lists_its_prompts_still_serves_its_tools() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let mut config = broken_prompt_list_server(temp_dir.path(), None);
    // `prompts/list` waits this long; the server answers everything else
    // at once.
    config.startup_timeout_ms = Some(1_000);

    let registry = connected_registry(&[config], None);

    assert_eq!(
        registry.server_statuses(),
        [McpServerStatus {
            name: "broken".to_string(),
            state: McpServerState::Ready,
            prompts_error: Some("MCP request 'prompts/list' timed out after 1s".to_string()),
            errors: Vec::new(),
        }]
    );
    assert!(registry.errors().is_empty(), "{:?}", registry.errors());
    assert!(registry.prompts().is_empty());
    let echo = registry
        .resolve_tool("mcp__broken__echo")
        .expect("the echo tool is registered");
    let first = registry
        .call_tool(&echo, serde_json::json!({}))
        .expect("the first call reaches a live server");
    assert_eq!(first.output, "echoed");
    // The server was started again once and not asked for its prompts
    // again, so the unanswered request cost one start-up timeout.
    assert_eq!(
        logged_methods(temp_dir.path()),
        [
            "initialize",
            "notifications/initialized",
            "tools/list",
            "prompts/list",
            "initialize",
            "notifications/initialized",
            "tools/list",
            "tools/call",
        ]
    );
}

#[cfg(unix)]
#[test]
fn a_prompt_list_failure_restarts_the_server_only_when_it_stopped_the_server() {
    for (reply, error, restarted) in [
        // An answer without a result stops the stdio transport.
        (
            r#"{"jsonrpc":"2.0","id":%s}"#,
            "MCP request 'prompts/list' missing result",
            true,
        ),
        // An error answer leaves it running.
        (
            r#"{"jsonrpc":"2.0","id":%s,"error":{"code":-32601,"message":"Method not found"}}"#,
            r#"MCP request 'prompts/list' failed: {"code":-32601,"message":"Method not found"}"#,
            false,
        ),
    ] {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let config = broken_prompt_list_server(temp_dir.path(), Some(reply));

        let registry = connected_registry(&[config], None);

        assert_eq!(
            registry.server_statuses(),
            [McpServerStatus {
                name: "broken".to_string(),
                state: McpServerState::Ready,
                prompts_error: Some(error.to_string()),
                errors: Vec::new(),
            }],
            "{reply}"
        );
        assert!(
            registry.errors().is_empty(),
            "{reply}: {:?}",
            registry.errors()
        );
        let echo = registry
            .resolve_tool("mcp__broken__echo")
            .expect("the echo tool is registered");
        let first = registry
            .call_tool(&echo, serde_json::json!({}))
            .unwrap_or_else(|error| panic!("the first call failed after {reply}: {error}"));
        assert_eq!(first.output, "echoed", "{reply}");
        let restart: &[&str] = if restarted {
            &["initialize", "notifications/initialized", "tools/list"]
        } else {
            &[]
        };
        let expected = [
            "initialize",
            "notifications/initialized",
            "tools/list",
            "prompts/list",
        ]
        .iter()
        .chain(restart)
        .chain(&["tools/call"])
        .map(|method| method.to_string())
        .collect::<Vec<_>>();
        assert_eq!(logged_methods(temp_dir.path()), expected, "{reply}");
    }
}

/// A stdio server that serves the tool `echo`, and answers the first
/// call to it with no result, which stops the stdio transport; it
/// answers the calls after that. Each message it gets is added to
/// `<dir>/requests`.
#[cfg(unix)]
fn tool_server_that_breaks_once(dir: &std::path::Path) -> McpServerConfig {
    let script = dir.join("breaks_once.sh");
    std::fs::write(
            &script,
            r#"#!/bin/sh
state_dir="$1"
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$state_dir/requests"
  id=${line#*'"id":'}
  id=${id%%,*}
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"breaks","version":"1"}}}\n' "$id"
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","inputSchema":{"type":"object"}}]}}\n' "$id"
      ;;
    *'"method":"tools/call"'*)
      if [ -f "$state_dir/broke" ]; then
        printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"echoed"}],"isError":false}}\n' "$id"
      else
        : > "$state_dir/broke"
        printf '{"jsonrpc":"2.0","id":%s}\n' "$id"
      fi
      ;;
  esac
done
"#,
        )
        .expect("write MCP fixture");
    let mut config = stdio_fixture_config("breaks", &script);
    config.args.push(dir.to_string_lossy().into_owned());
    config.tool_timeout_ms = Some(STDIO_TEST_STARTUP_TIMEOUT_MS);
    config
}

#[cfg(unix)]
#[test]
fn a_tool_call_that_stops_the_server_reconnects_before_returning() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let registry = connected_registry(&[tool_server_that_breaks_once(temp_dir.path())], None);
    let echo = registry
        .resolve_tool("mcp__breaks__echo")
        .expect("the echo tool");

    let first = registry
        .call_tool(&echo, serde_json::json!({}))
        .expect_err("the first call is answered with no result");
    let logged_by_then = logged_methods(temp_dir.path());
    let second = registry.call_tool(&echo, serde_json::json!({}));

    assert_eq!(first, "MCP request 'tools/call' missing result");
    // The next call goes to a new server, instead of failing on a broken
    // pipe to the stopped one, and the failed call is not sent again.
    let second = second.unwrap_or_else(|error| panic!("the next call failed: {error}"));
    assert_eq!(second.output, "echoed");
    let started_again = ["initialize", "notifications/initialized", "tools/list"];
    let expected = started_again
        .iter()
        .chain(&["tools/call"])
        .chain(&started_again)
        .map(|method| method.to_string())
        .collect::<Vec<_>>();
    assert_eq!(logged_by_then, expected, "started again before it returned");
    assert_eq!(
        logged_methods(temp_dir.path()),
        [expected, vec!["tools/call".to_string()]].concat()
    );
}

#[cfg(unix)]
#[test]
fn two_failures_on_one_transport_restart_its_server_once() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let config = listing_server_config(
        "x",
        temp_dir.path(),
        r#"[{"name":"echo","inputSchema":{"type":"object"}}]"#,
    );
    let registry = connected_registry(&[config], None);
    let client = registry.client("x").expect("the server's client");
    // Two requests failed on the same transport, and each reconnects
    // the server, at once.
    let failed = client.lock_transport().expect("its transport").clone();
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let reconnects = [(); 2].map(|_| {
        let client = Arc::clone(&client);
        let failed = Arc::clone(&failed);
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || {
            barrier.wait();
            client.reconnect(&failed, None, &|| false)
        })
    });
    for reconnect in reconnects {
        reconnect
            .join()
            .expect("a reconnect's thread")
            .expect("reconnect the server");
    }

    // The one that came second found the server started again, and left
    // it running.
    let starts = start_pids(temp_dir.path(), "x");
    assert_eq!(starts.len(), 2, "{starts:?}");
    assert!(
        !process_is_alive(&starts[0]),
        "the failed server still runs"
    );
    assert!(
        process_is_alive(&starts[1]),
        "the restarted server was stopped"
    );
    let current = client.lock_transport().expect("its transport").clone();
    assert!(!Arc::ptr_eq(&current, &failed));
    assert!(!current.is_closed());
}

/// A stdio server that serves its first start, answering its first tool
/// call with no result, which stops it; started again, it reads
/// `initialize` and exits without answering it.
#[cfg(unix)]
fn tool_server_that_cannot_start_again(dir: &std::path::Path) -> McpServerConfig {
    let script = dir.join("once.sh");
    std::fs::write(
            &script,
            r#"#!/bin/sh
state_dir="$1"
if [ -f "$state_dir/started" ]; then
  read -r line
  exit 0
fi
: > "$state_dir/started"
while IFS= read -r line; do
  id=${line#*'"id":'}
  id=${id%%,*}
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"once","version":"1"}}}\n' "$id"
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","inputSchema":{"type":"object"}}]}}\n' "$id"
      ;;
    *'"method":"tools/call"'*)
      printf '{"jsonrpc":"2.0","id":%s}\n' "$id"
      ;;
  esac
done
"#,
        )
        .expect("write MCP fixture");
    let mut config = stdio_fixture_config("once", &script);
    config.args.push(dir.to_string_lossy().into_owned());
    config.tool_timeout_ms = Some(STDIO_TEST_STARTUP_TIMEOUT_MS);
    config
}

#[cfg(unix)]
#[test]
fn a_stdio_server_that_cannot_start_again_after_a_call_is_failed() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let registry = connected_registry(
        &[tool_server_that_cannot_start_again(temp_dir.path())],
        None,
    );
    let echo = registry
        .resolve_tool("mcp__once__echo")
        .expect("the echo tool");
    let (_subscription, changes) = count_changes(&registry);

    let error = registry
        .call_tool(&echo, serde_json::json!({}))
        .expect_err("the call is answered with no result");

    assert_eq!(error, "MCP request 'tools/call' missing result");
    // Its server stopped, and could not be started again: the server
    // says why, rather than stay connected with every call failing.
    assert_eq!(
        registry.server_statuses(),
        [failed("once", "MCP server closed stdout")]
    );
    assert_eq!(changes.load(Ordering::SeqCst), 1, "no subscriber was told");
    assert!(registry.tools().is_empty());
    assert!(registry.client("once").is_none());
    assert_eq!(
        registry
            .call_tool(&echo, serde_json::json!({}))
            .expect_err("the server is not connected"),
        "MCP server 'once' is not connected"
    );
}

#[cfg(unix)]
#[test]
fn a_failed_reconnect_of_a_replaced_client_leaves_the_new_one() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let config = listing_server_config(
        "x",
        temp_dir.path(),
        r#"[{"name":"echo","inputSchema":{"type":"object"}}]"#,
    );
    let registry = connected_registry(&[config], None);
    let replaced = registry.client("x").expect("the first client");
    registry
        .reconnect_server("x")
        .expect("reconnect the server");
    let (_subscription, changes) = count_changes(&registry);

    // A request still under way on the replaced client failed, and its
    // reconnect could not start the server.
    *replaced.lock_reconnect_failure() = Some("MCP server closed stdout".to_string());
    registry.note_answer(
        "x",
        &replaced,
        &Err::<(), _>("MCP server closed stdout".to_string()),
    );

    assert_eq!(
        registry.server_statuses(),
        [status("x", McpServerState::Ready)]
    );
    assert_eq!(schema_names(&registry), ["mcp__x__echo"]);
    assert_eq!(changes.load(Ordering::SeqCst), 0);
}

#[test]
fn a_snapshot_never_mixes_two_states_of_a_server() {
    struct PromptingTransport;

    impl McpTransport for PromptingTransport {
        fn initialize(&self) -> Result<Value, String> {
            Ok(serde_json::json!({"capabilities": {"prompts": {}}}))
        }

        fn list_tools(&self, _cursor: Option<&str>) -> Result<Value, String> {
            Ok(serde_json::json!({"tools": [
                {"name": "echo", "inputSchema": {"type": "object"}}
            ]}))
        }

        fn call_tool(&self, _name: &str, _arguments: Value) -> Result<Value, String> {
            Err("not used".to_string())
        }

        fn list_resources(&self, _cursor: Option<&str>) -> Result<Value, String> {
            Err("not used".to_string())
        }

        fn list_resource_templates(&self, _cursor: Option<&str>) -> Result<Value, String> {
            Err("not used".to_string())
        }

        fn read_resource(&self, _uri: &str) -> Result<Value, String> {
            Err("not used".to_string())
        }

        fn list_prompts(&self, _cursor: Option<&str>) -> Result<Value, String> {
            Ok(serde_json::json!({"prompts": [{"name": "review"}]}))
        }
    }

    let config = McpServerConfig {
        name: "x".to_string(),
        ..Default::default()
    };
    let registry = McpRegistry::from_inner(McpRegistryInner {
        servers: vec![McpServerEntry {
            name: "x".to_string(),
            config: config.clone(),
            state: McpServerState::Starting,
            generation: FIRST_CONNECTION,
            stop: Arc::default(),
            tools: Vec::new(),
            prompts: Vec::new(),
            prompts_error: None,
            errors: Vec::new(),
        }],
        ..Default::default()
    });
    // The server connects and fails, over and over, while the registry
    // is read.
    let done = Arc::new(AtomicBool::new(false));
    let flipper = std::thread::spawn({
        let registry = registry.clone();
        let done = Arc::clone(&done);
        move || {
            for _ in 0..5_000 {
                let connected = connect_server_with_transport(
                    &config,
                    "x",
                    None,
                    Arc::new(PromptingTransport),
                    Arc::default(),
                );
                let _ = registry.finish_connecting("x", FIRST_CONNECTION, connected);
                let _ = registry.finish_connecting("x", FIRST_CONNECTION, Err("down".to_string()));
            }
            done.store(true, Ordering::SeqCst);
        }
    });

    let mut read = 0;
    while !done.load(Ordering::SeqCst) {
        let snapshot = registry.snapshot();
        let ready = snapshot.servers[0].state == McpServerState::Ready;
        assert_eq!(ready, !snapshot.tools.is_empty(), "{snapshot:?}");
        assert_eq!(ready, !snapshot.prompts.is_empty(), "{snapshot:?}");
        read += 1;
    }
    flipper.join().expect("the flipping thread");
    assert!(read > 0);
}

/// A stdio server whose lists come a page at a time. It declares prompts
/// and resources, and answers a list request with the result in
/// `<dir>/<name>/<list>-<cursor>.json`, where `<list>` is `tools`,
/// `prompts`, `resources` or `templates`, and `<cursor>` is the cursor
/// asked for, or `start` for the first page. A `tools/list` it has no page
/// for gets one tool, `t<n>` for its `n`th list request, and a new cursor,
/// `c<n>`, so that the list never ends; any other list it has no page for
/// is empty. Each message it gets is added to `<dir>/<name>/requests`.
#[cfg(unix)]
fn paged_server_config(
    name: &str,
    dir: &std::path::Path,
    pages: &[(&str, Value)],
) -> McpServerConfig {
    let state = dir.join(name);
    std::fs::create_dir_all(&state).expect("state dir");
    for (page, result) in pages {
        std::fs::write(state.join(format!("{page}.json")), result.to_string())
            .expect("write a page");
    }
    let script = dir.join(format!("{name}.sh"));
    std::fs::write(
            &script,
            r#"#!/bin/sh
state_dir="$1"
listed=0
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$state_dir/requests"
  id=${line#*'"id":'}
  id=${id%%,*}
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2024-11-05","capabilities":{"prompts":{},"resources":{}},"serverInfo":{"name":"paged","version":"1"}}}\n' "$id"
      continue
      ;;
    *'"method":"tools/list"'*) list=tools ;;
    *'"method":"prompts/list"'*) list=prompts ;;
    *'"method":"resources/list"'*) list=resources ;;
    *'"method":"resources/templates/list"'*) list=templates ;;
    *) continue ;;
  esac
  listed=$((listed + 1))
  cursor=start
  case "$line" in
    *'"cursor":"'*)
      cursor=${line#*'"cursor":"'}
      cursor=${cursor%%'"'*}
      ;;
  esac
  if [ -f "$state_dir/$list-$cursor.json" ]; then
    page=$(cat "$state_dir/$list-$cursor.json")
  elif [ "$list" = tools ]; then
    page="{\"tools\":[{\"name\":\"t$listed\",\"inputSchema\":{\"type\":\"object\"}}],\"nextCursor\":\"c$listed\"}"
  else
    page='{}'
  fi
  printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$id" "$page"
done
"#,
        )
        .expect("write MCP fixture");
    let mut config = stdio_fixture_config(name, &script);
    config.args.push(state.to_string_lossy().into_owned());
    config
}

/// A tool named `name`, as a server lists it.
#[cfg(unix)]
fn listed_tool(name: &str) -> Value {
    serde_json::json!({"name": name, "inputSchema": {"type": "object"}})
}

/// The params of each `method` request the paged server `name` got.
#[cfg(unix)]
fn list_params_sent(dir: &std::path::Path, name: &str, method: &str) -> Vec<Value> {
    prompt_server_requests(dir, name, method)
        .into_iter()
        .map(|request| request["params"].clone())
        .collect()
}

#[cfg(unix)]
#[test]
fn tools_are_listed_across_pages() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let config = paged_server_config(
        "paged",
        temp_dir.path(),
        &[
            (
                "tools-start",
                serde_json::json!({"tools": [listed_tool("a"), listed_tool("b")], "nextCursor": "c1"}),
            ),
            (
                "tools-c1",
                serde_json::json!({"tools": [listed_tool("c"), listed_tool("d")], "nextCursor": "c2"}),
            ),
            ("tools-c2", serde_json::json!({"tools": [listed_tool("e")]})),
        ],
    );

    let registry = connected_registry(&[config], None);

    assert_eq!(
        schema_names(&registry),
        [
            "mcp__paged__a",
            "mcp__paged__b",
            "mcp__paged__c",
            "mcp__paged__d",
            "mcp__paged__e"
        ]
    );
    assert_eq!(
        registry.server_statuses(),
        [status("paged", McpServerState::Ready)]
    );
    assert_eq!(
        list_params_sent(temp_dir.path(), "paged", "tools/list"),
        [
            serde_json::json!({}),
            serde_json::json!({"cursor": "c1"}),
            serde_json::json!({"cursor": "c2"})
        ]
    );
}

#[cfg(unix)]
#[test]
fn a_repeated_cursor_stops_listing() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let config = paged_server_config(
        "paged",
        temp_dir.path(),
        &[
            (
                "tools-start",
                serde_json::json!({"tools": [listed_tool("a")], "nextCursor": "c1"}),
            ),
            // It names the cursor of this very page again.
            (
                "tools-c1",
                serde_json::json!({"tools": [listed_tool("b")], "nextCursor": "c1"}),
            ),
        ],
    );

    let registry = connected_registry(&[config], None);

    assert_eq!(
        list_params_sent(temp_dir.path(), "paged", "tools/list").len(),
        2
    );
    assert_eq!(schema_names(&registry), ["mcp__paged__a", "mcp__paged__b"]);
    assert_eq!(
        registry.server_statuses(),
        [McpServerStatus {
            errors: vec![
                "incomplete tools/list result for 'paged': MCP server repeated a list cursor"
                    .to_string()
            ],
            ..status("paged", McpServerState::Ready)
        }]
    );
}

#[cfg(unix)]
#[test]
fn listing_stops_after_100_pages() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    // With no pages of its own, it names a new cursor on every page.
    let config = paged_server_config("paged", temp_dir.path(), &[]);

    let registry = connected_registry(&[config], None);

    assert_eq!(
        list_params_sent(temp_dir.path(), "paged", "tools/list").len(),
        100
    );
    // The pages read are kept.
    assert_eq!(registry.tools().len(), 100);
    assert_eq!(
        registry.server_statuses(),
        [McpServerStatus {
            errors: vec![
                "incomplete tools/list result for 'paged': MCP list stopped after 100 pages"
                    .to_string()
            ],
            ..status("paged", McpServerState::Ready)
        }]
    );
}

#[cfg(unix)]
#[test]
fn prompts_and_resources_are_listed_across_pages() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let config = paged_server_config(
        "paged",
        temp_dir.path(),
        &[
            ("tools-start", serde_json::json!({"tools": []})),
            (
                "prompts-start",
                serde_json::json!({"prompts": [{"name": "first"}], "nextCursor": "p1"}),
            ),
            (
                "prompts-p1",
                serde_json::json!({"prompts": [{"name": "second"}]}),
            ),
            (
                "resources-start",
                serde_json::json!({"resources": [{"uri": "memo://1", "name": "one"}], "nextCursor": "r1"}),
            ),
            (
                "resources-r1",
                serde_json::json!({"resources": [{"uri": "memo://2", "name": "two"}]}),
            ),
            (
                "templates-start",
                serde_json::json!({"resourceTemplates": [{"uriTemplate": "memo://{a}", "name": "a"}], "nextCursor": "t1"}),
            ),
            (
                "templates-t1",
                serde_json::json!({"resourceTemplates": [{"uriTemplate": "memo://{b}", "name": "b"}]}),
            ),
        ],
    );

    let registry = connected_registry(&[config], None);
    let resources = registry
        .list_resources(Some("paged"))
        .expect("list the resources");
    let templates = registry.list_resource_templates_with_errors(None);

    assert_eq!(prompt_names(&registry), ["first", "second"]);
    assert_eq!(
        resources
            .iter()
            .map(|resource| resource.uri.as_str())
            .collect::<Vec<_>>(),
        ["memo://1", "memo://2"]
    );
    assert!(templates.errors.is_empty(), "{:?}", templates.errors);
    assert_eq!(
        templates
            .resource_templates
            .iter()
            .map(|template| template.uri_template.as_str())
            .collect::<Vec<_>>(),
        ["memo://{a}", "memo://{b}"]
    );
    for (method, cursor) in [
        ("prompts/list", "p1"),
        ("resources/list", "r1"),
        ("resources/templates/list", "t1"),
    ] {
        assert_eq!(
            list_params_sent(temp_dir.path(), "paged", method),
            [serde_json::json!({}), serde_json::json!({"cursor": cursor})],
            "{method}"
        );
    }
}

#[cfg(unix)]
#[test]
fn a_resource_list_cut_short_says_so_where_its_errors_go() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let config = paged_server_config(
        "paged",
        temp_dir.path(),
        &[
            ("tools-start", serde_json::json!({"tools": []})),
            (
                "resources-start",
                serde_json::json!({"resources": [{"uri": "memo://1", "name": "one"}], "nextCursor": "r1"}),
            ),
            (
                "resources-r1",
                serde_json::json!({"resources": [{"uri": "memo://2", "name": "two"}], "nextCursor": "r1"}),
            ),
            (
                "templates-start",
                serde_json::json!({"resourceTemplates": [{"uriTemplate": "memo://{a}", "name": "a"}], "nextCursor": "t1"}),
            ),
            (
                "templates-t1",
                serde_json::json!({"resourceTemplates": [{"uriTemplate": "memo://{b}", "name": "b"}], "nextCursor": "t1"}),
            ),
        ],
    );
    let registry = connected_registry(&[config], None);
    let (_subscription, changes) = count_changes(&registry);
    let uris = |resources: &[McpResource]| {
        resources
            .iter()
            .map(|resource| resource.uri.clone())
            .collect::<Vec<_>>()
    };

    // A listing with errors keeps what it read, and says why it stopped.
    let listing = registry.list_resources_with_errors(Some("paged"));
    assert_eq!(uris(&listing.resources), ["memo://1", "memo://2"]);
    assert_eq!(listing.errors, ["paged: MCP server repeated a list cursor"]);
    assert_eq!(changes.load(Ordering::SeqCst), 0);

    // One without errors of its own keeps what it read too, and the
    // server's errors say why it stopped.
    let resources = registry
        .list_resources(Some("paged"))
        .expect("the resources read");
    let templates = registry
        .list_resource_templates(Some("paged"))
        .expect("the resource templates read");

    assert_eq!(uris(&resources), ["memo://1", "memo://2"]);
    assert_eq!(
        templates
            .iter()
            .map(|template| template.uri_template.as_str())
            .collect::<Vec<_>>(),
        ["memo://{a}", "memo://{b}"]
    );
    assert_eq!(
            registry.server_statuses(),
            [McpServerStatus {
                errors: vec![
                    "incomplete resources/list result for 'paged': MCP server repeated a list cursor"
                        .to_string(),
                    "incomplete resources/templates/list result for 'paged': MCP server repeated a list cursor"
                        .to_string(),
                ],
                ..status("paged", McpServerState::Ready)
            }]
        );
    assert_eq!(changes.load(Ordering::SeqCst), 2);
    // Noted already: listing again changes nothing, and tells no one.
    registry
        .list_resources(Some("paged"))
        .expect("the resources read again");
    assert_eq!(registry.server_statuses()[0].errors.len(), 2);
    assert_eq!(changes.load(Ordering::SeqCst), 2);
}

#[cfg(unix)]
#[test]
fn an_empty_cursor_ends_a_list() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let config = paged_server_config(
        "paged",
        temp_dir.path(),
        &[(
            "tools-start",
            serde_json::json!({"tools": [listed_tool("a")], "nextCursor": ""}),
        )],
    );

    let registry = connected_registry(&[config], None);

    assert_eq!(
        list_params_sent(temp_dir.path(), "paged", "tools/list"),
        [serde_json::json!({})]
    );
    assert_eq!(schema_names(&registry), ["mcp__paged__a"]);
    assert_eq!(
        registry.server_statuses(),
        [status("paged", McpServerState::Ready)]
    );
}
