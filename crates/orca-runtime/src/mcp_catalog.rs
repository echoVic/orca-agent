//! The MCP catalog of a thread's typed surface: every configured server and
//! how it stands, and the tools and prompts of the connected ones, built
//! from the session's MCP registry.

use std::collections::{BTreeMap, BTreeSet};

use orca_core::mcp_types::{McpPrompt, McpTool};
use orca_mcp::{McpRegistry, McpServerState, McpServerStatus};
use serde_json::{Map, Value};

use crate::agent_continuation::canonical_json_bytes;
use crate::runtime_surface::{
    DisplayText, FiniteF64, McpCatalogRevision, NonEmptyText, NonEmptyVec, Sha256Digest,
    SurfaceCatalogEntryId, SurfaceMcpCatalogDiagnostic, SurfaceMcpCatalogDiagnosticCode,
    SurfaceMcpCatalogEntryKind, SurfaceMcpCatalogSnapshot, SurfaceMcpPrompt,
    SurfaceMcpPromptArgument, SurfaceMcpServer, SurfaceMcpServerStatus, SurfaceMcpTool,
    SurfaceSchema, SurfaceSchemaInteger, SurfaceSchemaProperty,
};

/// The catalog that follows `current`, or `None` while `registry` still
/// matches it.
pub(crate) fn next_mcp_catalog(
    registry: &McpRegistry,
    current: &SurfaceMcpCatalogSnapshot,
) -> Option<SurfaceMcpCatalogSnapshot> {
    let mut next = mcp_catalog(registry, current.revision);
    if next == *current {
        return None;
    }
    next.revision = McpCatalogRevision::try_new(current.revision.get().checked_add(1)?).ok()?;
    Some(next)
}

/// The catalog of `registry` at its first revision, for MCP servers that no
/// thread has taken yet: it lists them as the thread that takes them will.
/// The TUI uses it to show the servers it starts at launch.
pub fn mcp_catalog_snapshot(registry: &McpRegistry) -> SurfaceMcpCatalogSnapshot {
    mcp_catalog(
        registry,
        McpCatalogRevision::try_new(1).expect("one is a valid revision"),
    )
}

/// The catalog `registry` stands for, at `revision`. Resources are not
/// listed yet.
fn mcp_catalog(registry: &McpRegistry, revision: McpCatalogRevision) -> SurfaceMcpCatalogSnapshot {
    catalog_of(
        registry.server_statuses(),
        registry.tools(),
        registry.prompts(),
        revision,
    )
}

fn catalog_of(
    statuses: Vec<McpServerStatus>,
    registry_tools: Vec<McpTool>,
    registry_prompts: Vec<McpPrompt>,
    revision: McpCatalogRevision,
) -> SurfaceMcpCatalogSnapshot {
    let mut tools = Vec::new();
    let mut diagnostics = Vec::new();
    let mut source_indices = BTreeMap::<String, u64>::new();
    let mut omitted = BTreeMap::<String, u64>::new();
    for tool in registry_tools {
        let source_index = source_indices.entry(tool.server.clone()).or_default();
        let index = *source_index;
        *source_index += 1;
        match surface_tool(&tool) {
            Ok(tool) => tools.push(tool),
            Err(codes) => {
                // A tool always names the server it came from.
                let Ok(server) = NonEmptyText::try_new(tool.server.clone()) else {
                    continue;
                };
                *omitted.entry(tool.server.clone()).or_default() += 1;
                let source_digest = Sha256Digest::digest(
                    serde_json::to_value(&tool)
                        .ok()
                        .and_then(|source| canonical_json_bytes(&source).ok())
                        .unwrap_or_default(),
                );
                diagnostics.extend(codes.into_iter().map(|code| SurfaceMcpCatalogDiagnostic {
                    server: server.clone(),
                    entry_kind: SurfaceMcpCatalogEntryKind::Tool,
                    source_index: index,
                    code,
                    source_digest,
                }));
            }
        }
    }
    diagnostics.sort_by(|left, right| {
        (
            &left.server,
            left.source_index,
            diagnostic_code_rank(left.code),
        )
            .cmp(&(
                &right.server,
                right.source_index,
                diagnostic_code_rank(right.code),
            ))
    });
    let servers = statuses
        .into_iter()
        .filter_map(|server| {
            let status = match (
                surface_server_status(server.state),
                omitted.get(&server.name),
            ) {
                // A server some of whose tools cannot be shown has failed.
                (SurfaceMcpServerStatus::Ready, Some(&count)) => SurfaceMcpServerStatus::Failed {
                    message: DisplayText::new(if count == 1 {
                        "1 tool could not be listed".to_string()
                    } else {
                        format!("{count} tools could not be listed")
                    }),
                },
                (status, _) => status,
            };
            Some(SurfaceMcpServer {
                name: NonEmptyText::try_new(server.name).ok()?,
                status,
                prompts_error: server.prompts_error.map(DisplayText::new),
            })
        })
        .collect();
    SurfaceMcpCatalogSnapshot {
        revision,
        servers,
        tools,
        prompts: registry_prompts
            .into_iter()
            .filter_map(surface_prompt)
            .collect(),
        resources: Vec::new(),
        resource_templates: Vec::new(),
        diagnostics,
    }
}

fn surface_server_status(state: McpServerState) -> SurfaceMcpServerStatus {
    match state {
        McpServerState::Starting => SurfaceMcpServerStatus::Starting,
        McpServerState::Ready => SurfaceMcpServerStatus::Ready,
        McpServerState::Failed { message } => SurfaceMcpServerStatus::Failed {
            message: DisplayText::new(message),
        },
        McpServerState::NeedsLogin => SurfaceMcpServerStatus::NeedsLogin,
        McpServerState::Disabled => SurfaceMcpServerStatus::Disabled,
    }
}

/// The surface entry for `tool`, or the fields that cannot make one.
fn surface_tool(tool: &McpTool) -> Result<SurfaceMcpTool, Vec<SurfaceMcpCatalogDiagnosticCode>> {
    let server = NonEmptyText::try_new(tool.server.clone());
    let name = NonEmptyText::try_new(tool.name.clone());
    let schema_name = NonEmptyText::try_new(tool.schema_name.clone());
    let id = SurfaceCatalogEntryId::try_new(format!("mcp-tool:{}", tool.schema_name));
    match (server, name, schema_name, id) {
        (Ok(server), Ok(name), Ok(schema_name), Ok(id)) => Ok(SurfaceMcpTool {
            id,
            server,
            name,
            schema_name,
            description: tool.description.clone().map(DisplayText::new),
            input_schema: surface_schema(&tool.input_schema),
            read_only: tool.read_only,
        }),
        (_, name, schema_name, _) => {
            let mut codes = Vec::new();
            if name.is_err() {
                codes.push(SurfaceMcpCatalogDiagnosticCode::EmptyName);
            }
            if schema_name.is_err() {
                codes.push(SurfaceMcpCatalogDiagnosticCode::EmptySchemaName);
            }
            Err(codes)
        }
    }
}

/// The surface entry for `prompt`. The registry lists no prompt without a
/// name, or with an argument without one, so every prompt makes an entry.
fn surface_prompt(prompt: McpPrompt) -> Option<SurfaceMcpPrompt> {
    Some(SurfaceMcpPrompt {
        server: NonEmptyText::try_new(prompt.server).ok()?,
        name: NonEmptyText::try_new(prompt.name).ok()?,
        description: prompt.description.map(DisplayText::new),
        arguments: prompt
            .arguments
            .into_iter()
            .map(|argument| {
                Some(SurfaceMcpPromptArgument {
                    name: NonEmptyText::try_new(argument.name).ok()?,
                    description: argument.description.map(DisplayText::new),
                    required: argument.required,
                })
            })
            .collect::<Option<_>>()?,
    })
}

fn diagnostic_code_rank(code: SurfaceMcpCatalogDiagnosticCode) -> u8 {
    match code {
        SurfaceMcpCatalogDiagnosticCode::EmptyName => 0,
        SurfaceMcpCatalogDiagnosticCode::EmptySchemaName => 1,
        SurfaceMcpCatalogDiagnosticCode::InvalidUri => 2,
        SurfaceMcpCatalogDiagnosticCode::InvalidUriTemplate => 3,
        SurfaceMcpCatalogDiagnosticCode::InvalidMime => 4,
        SurfaceMcpCatalogDiagnosticCode::InvalidSchema => 5,
    }
}

/// Keywords that describe a schema without constraining a value.
const ANNOTATIONS: [&str; 4] = ["$schema", "$comment", "default", "examples"];

/// The closed form of a JSON schema. A level the closed form cannot carry
/// becomes an `Unsupported` descriptor that names the keywords in the way;
/// the levels around it keep their shape.
fn surface_schema(schema: &Value) -> SurfaceSchema {
    closed_schema(schema).unwrap_or_else(|keywords| SurfaceSchema::Unsupported {
        schema_digest: Sha256Digest::digest(canonical_json_bytes(schema).unwrap_or_default()),
        unsupported_keywords: keywords,
    })
}

fn closed_schema(schema: &Value) -> Result<SurfaceSchema, NonEmptyVec<NonEmptyText>> {
    let Value::Object(schema) = schema else {
        return Err(keyword_list(BTreeSet::from(["type".to_string()])));
    };
    let mut level = SchemaLevel {
        schema,
        unsupported: BTreeSet::new(),
    };
    let title = level.text("title");
    let description = level.text("description");
    let kind = match schema.get("type") {
        Some(Value::String(kind)) => kind.as_str(),
        _ => "",
    };
    let known: &[&str] = match kind {
        "string" => &["enum", "minLength", "maxLength"],
        "integer" => &["enum", "minimum", "maximum"],
        "number" => &["minimum", "maximum"],
        "boolean" => &[],
        "array" => &["items", "minItems", "maxItems"],
        "object" => &["properties", "required", "additionalProperties"],
        _ => {
            level.unsupported("type");
            &[]
        }
    };
    for keyword in schema.keys() {
        if !matches!(keyword.as_str(), "type" | "title" | "description")
            && !ANNOTATIONS.contains(&keyword.as_str())
            && !known.contains(&keyword.as_str())
        {
            level.unsupported(keyword);
        }
    }
    let closed = match kind {
        "string" => {
            let enum_values = level.enum_values("enum", |value| {
                value
                    .as_str()
                    .map(|value| DisplayText::new(value.to_string()))
            });
            let (min_length, max_length) = level.ordered_counts("minLength", "maxLength");
            SurfaceSchema::String {
                title,
                description,
                enum_values,
                min_length,
                max_length,
            }
        }
        "integer" => {
            let enum_values = level.enum_values("enum", schema_integer);
            let minimum = level.value("minimum", schema_integer);
            let maximum = level.value("maximum", schema_integer);
            if let (Some(minimum), Some(maximum)) = (&minimum, &maximum)
                && integer_value(minimum) > integer_value(maximum)
            {
                level.unsupported("minimum");
                level.unsupported("maximum");
            }
            SurfaceSchema::Integer {
                title,
                description,
                minimum,
                maximum,
                enum_values,
            }
        }
        "number" => {
            let number = |value: &Value| FiniteF64::try_new(value.as_f64()?).ok();
            let minimum = level.value("minimum", number);
            let maximum = level.value("maximum", number);
            if let (Some(minimum), Some(maximum)) = (minimum, maximum)
                && minimum.get() > maximum.get()
            {
                level.unsupported("minimum");
                level.unsupported("maximum");
            }
            SurfaceSchema::Number {
                title,
                description,
                minimum,
                maximum,
            }
        }
        "boolean" => SurfaceSchema::Boolean { title, description },
        "array" => {
            let items = match schema.get("items") {
                Some(items @ Value::Object(_)) => surface_schema(items),
                _ => {
                    level.unsupported("items");
                    // Never returned: the level is unsupported.
                    SurfaceSchema::Boolean {
                        title: None,
                        description: None,
                    }
                }
            };
            let (min_items, max_items) = level.ordered_counts("minItems", "maxItems");
            SurfaceSchema::Array {
                title,
                description,
                items: Box::new(items),
                min_items,
                max_items,
            }
        }
        "object" => {
            let no_properties = Map::new();
            let properties = match schema.get("properties") {
                None => &no_properties,
                Some(Value::Object(properties)) => properties,
                Some(_) => {
                    level.unsupported("properties");
                    &no_properties
                }
            };
            let required = match schema.get("required") {
                None => BTreeSet::new(),
                Some(Value::Array(names)) => {
                    let required = names
                        .iter()
                        .map(|name| name.as_str().filter(|name| properties.contains_key(*name)))
                        .collect::<Option<BTreeSet<_>>>();
                    match required {
                        Some(required) if required.len() == names.len() => required,
                        _ => {
                            level.unsupported("required");
                            BTreeSet::new()
                        }
                    }
                }
                Some(_) => {
                    level.unsupported("required");
                    BTreeSet::new()
                }
            };
            // The closed form denies properties the schema does not declare;
            // a schema that explicitly admits them cannot be carried.
            if !matches!(
                schema.get("additionalProperties"),
                None | Some(Value::Bool(false))
            ) {
                level.unsupported("additionalProperties");
            }
            SurfaceSchema::Object {
                title,
                description,
                properties: properties
                    .iter()
                    .map(|(name, schema)| SurfaceSchemaProperty {
                        name: DisplayText::new(name.clone()),
                        required: required.contains(name.as_str()),
                        schema: Box::new(surface_schema(schema)),
                    })
                    .collect(),
                additional_properties: (),
            }
        }
        // Never returned: an unknown type is unsupported.
        _ => SurfaceSchema::Boolean { title, description },
    };
    if level.unsupported.is_empty() {
        Ok(closed)
    } else {
        Err(keyword_list(level.unsupported))
    }
}

/// One level of a schema being read, and the keywords found at it that the
/// closed form cannot carry.
struct SchemaLevel<'a> {
    schema: &'a Map<String, Value>,
    unsupported: BTreeSet<String>,
}

impl SchemaLevel<'_> {
    fn unsupported(&mut self, keyword: &str) {
        self.unsupported.insert(keyword.to_string());
    }

    fn value<T>(&mut self, keyword: &str, read: impl Fn(&Value) -> Option<T>) -> Option<T> {
        let value = read(self.schema.get(keyword)?);
        if value.is_none() {
            self.unsupported(keyword);
        }
        value
    }

    fn text(&mut self, keyword: &str) -> Option<DisplayText> {
        self.value(keyword, |value| {
            value
                .as_str()
                .map(|text| DisplayText::new(text.to_string()))
        })
    }

    /// A lower and an upper count, when both are counts in order.
    fn ordered_counts(&mut self, lower: &str, upper: &str) -> (Option<u64>, Option<u64>) {
        let minimum = self.value(lower, Value::as_u64);
        let maximum = self.value(upper, Value::as_u64);
        if let (Some(minimum), Some(maximum)) = (minimum, maximum)
            && minimum > maximum
        {
            self.unsupported(lower);
            self.unsupported(upper);
        }
        (minimum, maximum)
    }

    /// The members of an enum, when each reads as a `T` and none repeats.
    fn enum_values<T: PartialEq>(
        &mut self,
        keyword: &str,
        read: impl Fn(&Value) -> Option<T>,
    ) -> Vec<T> {
        let Some(members) = self.schema.get(keyword) else {
            return Vec::new();
        };
        let values = members
            .as_array()
            .and_then(|members| members.iter().map(&read).collect::<Option<Vec<_>>>())
            .filter(|values| {
                values
                    .iter()
                    .enumerate()
                    .all(|(index, value)| !values[..index].contains(value))
            });
        values.unwrap_or_else(|| {
            self.unsupported(keyword);
            Vec::new()
        })
    }
}

fn schema_integer(value: &Value) -> Option<SurfaceSchemaInteger> {
    match (value.as_u64(), value.as_i64()) {
        (Some(value), _) => Some(SurfaceSchemaInteger::non_negative(value)),
        (None, Some(value)) => SurfaceSchemaInteger::try_negative(value).ok(),
        (None, None) => None,
    }
}

fn integer_value(value: &SurfaceSchemaInteger) -> i128 {
    match value {
        SurfaceSchemaInteger::Negative(value) => i128::from(value.get()),
        SurfaceSchemaInteger::NonNegative(value) => i128::from(*value),
    }
}

fn keyword_list(keywords: BTreeSet<String>) -> NonEmptyVec<NonEmptyText> {
    NonEmptyVec::try_new(
        keywords
            .into_iter()
            .filter_map(|keyword| NonEmptyText::try_new(keyword).ok())
            .collect(),
    )
    .unwrap_or_else(|_| {
        NonEmptyVec::try_new(vec![
            NonEmptyText::try_new("schema").expect("the fallback keyword is not empty"),
        ])
        .expect("the fallback list is not empty")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::mcp_types::McpPromptArgument;

    fn text(value: &str) -> NonEmptyText {
        NonEmptyText::try_new(value).unwrap()
    }

    /// A connected server whose prompts were listed.
    fn ready(name: &str) -> McpServerStatus {
        McpServerStatus {
            name: name.to_string(),
            state: McpServerState::Ready,
            prompts_error: None,
            errors: Vec::new(),
        }
    }

    /// `name` as the catalog lists it with `status`, and no prompt-list
    /// error.
    fn listed(name: &str, status: SurfaceMcpServerStatus) -> SurfaceMcpServer {
        SurfaceMcpServer {
            name: text(name),
            status,
            prompts_error: None,
        }
    }

    #[test]
    fn a_level_the_closed_form_cannot_carry_is_unsupported_and_the_rest_keeps_its_shape() {
        let schema = serde_json::json!({
            "type": "object",
            "$schema": "http://json-schema.org/draft-07/schema#",
            "properties": {
                "path": {"type": "string", "pattern": "^/", "description": "where"},
                "tags": {"type": "array", "items": {"type": "string", "enum": ["a", "b"]}, "maxItems": 3}
            },
            "required": ["path"],
            "additionalProperties": false
        });

        let SurfaceSchema::Object { properties, .. } = surface_schema(&schema) else {
            panic!("the object keeps its closed form");
        };

        assert_eq!(properties.len(), 2);
        assert_eq!(properties[0].name, DisplayText::new("path"));
        assert!(properties[0].required);
        let SurfaceSchema::Unsupported {
            unsupported_keywords,
            schema_digest,
        } = properties[0].schema.as_ref()
        else {
            panic!("a pattern cannot be carried: {:?}", properties[0].schema);
        };
        assert_eq!(unsupported_keywords.as_slice(), [text("pattern")]);
        assert_eq!(
            *schema_digest,
            Sha256Digest::digest(canonical_json_bytes(&schema["properties"]["path"]).unwrap())
        );
        assert!(!properties[1].required);
        assert_eq!(
            *properties[1].schema,
            SurfaceSchema::Array {
                title: None,
                description: None,
                items: Box::new(SurfaceSchema::String {
                    title: None,
                    description: None,
                    enum_values: vec![DisplayText::new("a"), DisplayText::new("b")],
                    min_length: None,
                    max_length: None,
                }),
                min_items: None,
                max_items: Some(3),
            }
        );
    }

    #[test]
    fn schemas_that_admit_more_than_the_closed_form_are_unsupported() {
        for (schema, keywords) in [
            (
                serde_json::json!({"type": "object", "additionalProperties": true}),
                vec!["additionalProperties"],
            ),
            (
                serde_json::json!({"type": ["string", "null"]}),
                vec!["type"],
            ),
            (
                serde_json::json!({"anyOf": [{"type": "string"}]}),
                vec!["anyOf", "type"],
            ),
            (
                serde_json::json!({"type": "integer", "minimum": 5, "maximum": 1}),
                vec!["maximum", "minimum"],
            ),
            (
                serde_json::json!({"type": "string", "enum": ["a", "a"]}),
                vec!["enum"],
            ),
            (
                serde_json::json!({"type": "object", "properties": {}, "required": ["missing"]}),
                vec!["required"],
            ),
            (serde_json::json!(true), vec!["type"]),
        ] {
            let SurfaceSchema::Unsupported {
                unsupported_keywords,
                ..
            } = surface_schema(&schema)
            else {
                panic!("{schema} must be unsupported");
            };
            let expected = keywords.into_iter().map(text).collect::<Vec<_>>();
            assert_eq!(unsupported_keywords.as_slice(), expected, "{schema}");
        }
    }

    #[test]
    fn a_negative_integer_bound_keeps_its_value() {
        assert_eq!(
            surface_schema(&serde_json::json!({"type": "integer", "minimum": -3, "maximum": 4})),
            SurfaceSchema::Integer {
                title: None,
                description: None,
                minimum: Some(SurfaceSchemaInteger::try_negative(-3).unwrap()),
                maximum: Some(SurfaceSchemaInteger::non_negative(4)),
                enum_values: Vec::new(),
            }
        );
    }

    #[test]
    fn a_tool_that_is_not_read_only_serializes_as_before() {
        let tool = |read_only| SurfaceMcpTool {
            id: SurfaceCatalogEntryId::try_new("mcp-tool:mcp__docs__lookup").unwrap(),
            server: text("docs"),
            name: text("lookup"),
            schema_name: text("mcp__docs__lookup"),
            description: None,
            input_schema: SurfaceSchema::Boolean {
                title: None,
                description: None,
            },
            read_only,
        };

        let written = serde_json::to_value(tool(false)).unwrap();
        assert!(written.get("read_only").is_none(), "{written}");
        let read: SurfaceMcpTool = serde_json::from_value(written).unwrap();
        assert_eq!(read, tool(false));
        let written = serde_json::to_value(tool(true)).unwrap();
        assert_eq!(written["read_only"], true);
        assert_eq!(
            serde_json::from_value::<SurfaceMcpTool>(written).unwrap(),
            tool(true)
        );
    }

    #[test]
    fn a_tool_without_a_name_is_left_out_with_a_diagnostic() {
        let tool = |name: &str| McpTool {
            server: "docs".to_string(),
            name: name.to_string(),
            schema_name: format!("mcp__docs__{name}"),
            description: None,
            input_schema: serde_json::json!({"type": "object"}),
            read_only: false,
        };
        let catalog = catalog_of(
            vec![ready("docs"), ready("other")],
            vec![tool("lookup"), tool(" ")],
            Vec::new(),
            McpCatalogRevision::try_new(2).unwrap(),
        );

        assert_eq!(
            catalog.servers,
            [
                listed(
                    "docs",
                    SurfaceMcpServerStatus::Failed {
                        message: DisplayText::new("1 tool could not be listed"),
                    }
                ),
                listed("other", SurfaceMcpServerStatus::Ready),
            ]
        );
        assert_eq!(
            catalog
                .tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            ["lookup"]
        );
        assert_eq!(catalog.diagnostics.len(), 1);
        let diagnostic = &catalog.diagnostics[0];
        assert_eq!(diagnostic.server, text("docs"));
        assert_eq!(diagnostic.entry_kind, SurfaceMcpCatalogEntryKind::Tool);
        assert_eq!(diagnostic.source_index, 1);
        assert_eq!(diagnostic.code, SurfaceMcpCatalogDiagnosticCode::EmptyName);
        assert_eq!(
            diagnostic.source_digest,
            Sha256Digest::digest(
                canonical_json_bytes(&serde_json::to_value(tool(" ")).unwrap()).unwrap()
            )
        );
    }

    #[test]
    fn catalog_reports_starting_failed_and_needs_login() {
        let status = |name: &str, state, prompts_error: Option<&str>| McpServerStatus {
            name: name.to_string(),
            state,
            prompts_error: prompts_error.map(str::to_string),
            errors: Vec::new(),
        };
        let catalog = catalog_of(
            vec![
                status("slow", McpServerState::Starting, None),
                status(
                    "docs",
                    McpServerState::Ready,
                    Some("MCP request 'prompts/list' failed: boom"),
                ),
                status(
                    "broken",
                    McpServerState::Failed {
                        message: "connection refused".to_string(),
                    },
                    None,
                ),
                status("linear", McpServerState::NeedsLogin, None),
                status("archive", McpServerState::Disabled, None),
            ],
            Vec::new(),
            Vec::new(),
            McpCatalogRevision::try_new(2).unwrap(),
        );

        let server = |name: &str, status, prompts_error: Option<&str>| SurfaceMcpServer {
            name: text(name),
            status,
            prompts_error: prompts_error.map(DisplayText::new),
        };
        assert_eq!(
            catalog.servers,
            [
                server("slow", SurfaceMcpServerStatus::Starting, None),
                server(
                    "docs",
                    SurfaceMcpServerStatus::Ready,
                    Some("MCP request 'prompts/list' failed: boom"),
                ),
                server(
                    "broken",
                    SurfaceMcpServerStatus::Failed {
                        message: DisplayText::new("connection refused"),
                    },
                    None,
                ),
                server("linear", SurfaceMcpServerStatus::NeedsLogin, None),
                server("archive", SurfaceMcpServerStatus::Disabled, None),
            ]
        );
    }

    #[test]
    fn prompts_are_listed_with_their_arguments() {
        let catalog = catalog_of(
            vec![ready("docs")],
            Vec::new(),
            vec![
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
            ],
            McpCatalogRevision::try_new(2).unwrap(),
        );

        assert_eq!(
            catalog.prompts,
            [
                SurfaceMcpPrompt {
                    server: text("docs"),
                    name: text("review_pr"),
                    description: Some(DisplayText::new("Reviews a pull request")),
                    arguments: vec![
                        SurfaceMcpPromptArgument {
                            name: text("pr"),
                            description: Some(DisplayText::new("The pull request")),
                            required: true,
                        },
                        SurfaceMcpPromptArgument {
                            name: text("branch"),
                            description: None,
                            required: false,
                        },
                    ],
                },
                SurfaceMcpPrompt {
                    server: text("docs"),
                    name: text("summarize"),
                    description: None,
                    arguments: Vec::new(),
                },
            ]
        );
        assert_eq!(
            catalog.servers,
            [listed("docs", SurfaceMcpServerStatus::Ready)]
        );
    }

    #[test]
    fn a_catalog_without_prompts_leaves_them_off_the_wire() {
        let catalog = SurfaceMcpCatalogSnapshot {
            revision: McpCatalogRevision::try_new(3).unwrap(),
            servers: vec![listed("docs", SurfaceMcpServerStatus::Ready)],
            tools: Vec::new(),
            prompts: Vec::new(),
            resources: Vec::new(),
            resource_templates: Vec::new(),
            diagnostics: Vec::new(),
        };
        let without_prompts = serde_json::json!({
            "revision": 3,
            "servers": [{"name": "docs", "status": "Ready", "prompts_error": null}],
            "tools": [],
            "resources": [],
            "resource_templates": [],
            "diagnostics": []
        });

        assert_eq!(serde_json::to_value(&catalog).unwrap(), without_prompts);
        assert_eq!(
            serde_json::from_value::<SurfaceMcpCatalogSnapshot>(without_prompts).unwrap(),
            catalog
        );
        let with_prompts = SurfaceMcpCatalogSnapshot {
            prompts: vec![SurfaceMcpPrompt {
                server: text("docs"),
                name: text("review_pr"),
                description: None,
                arguments: vec![SurfaceMcpPromptArgument {
                    name: text("pr"),
                    description: None,
                    required: true,
                }],
            }],
            ..catalog
        };
        let written = serde_json::to_value(&with_prompts).unwrap();
        assert_eq!(
            serde_json::from_value::<SurfaceMcpCatalogSnapshot>(written).unwrap(),
            with_prompts
        );
    }
}
