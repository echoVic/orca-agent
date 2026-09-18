use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use orca_core::config::WorkflowConfig;
use orca_core::workflow_types::{
    WorkflowArgSpec, WorkflowArgType, WorkflowArgsSchema, WorkflowInput, WorkflowMeta,
};
use serde_json::{Map, Number, Value};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkflowScriptSource {
    ScriptPath,
    InlineScript,
    NamedWorkflow,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedWorkflowScript {
    pub source_kind: WorkflowScriptSource,
    pub original_path: Option<PathBuf>,
    pub persisted_path: PathBuf,
    pub meta: WorkflowMeta,
    pub args_schema: WorkflowArgsSchema,
    pub script: String,
    pub script_digest: String,
}

pub fn resolve_workflow_script(
    input: &WorkflowInput,
    cwd: &Path,
    session_dir: &Path,
) -> io::Result<ResolvedWorkflowScript> {
    let persisted_path = session_dir
        .join("workflows")
        .join("scripts")
        .join("script.js");
    resolve_workflow_script_to_path(input, cwd, &persisted_path)
}

pub fn resolve_workflow_script_to_path(
    input: &WorkflowInput,
    cwd: &Path,
    persisted_path: &Path,
) -> io::Result<ResolvedWorkflowScript> {
    let config_dir =
        orca_core::config::folder_trust::config_dir().unwrap_or_else(|| PathBuf::from(".orca"));
    let user_dir = config_dir.join("workflows");
    resolve_workflow_script_with_dirs_to_path(
        input,
        cwd,
        &user_dir,
        Some(&config_dir),
        persisted_path,
    )
}

pub fn resolve_workflow_script_with_user_dir(
    input: &WorkflowInput,
    cwd: &Path,
    session_dir: &Path,
    user_workflow_dir: &Path,
) -> io::Result<ResolvedWorkflowScript> {
    let persisted_path = session_dir
        .join("workflows")
        .join("scripts")
        .join("script.js");
    resolve_workflow_script_with_user_dir_to_path(input, cwd, user_workflow_dir, &persisted_path)
}

pub fn resolve_workflow_script_with_user_dir_and_config_dir(
    input: &WorkflowInput,
    cwd: &Path,
    session_dir: &Path,
    user_workflow_dir: &Path,
    config_dir: &Path,
) -> io::Result<ResolvedWorkflowScript> {
    let persisted_path = session_dir
        .join("workflows")
        .join("scripts")
        .join("script.js");
    resolve_workflow_script_with_dirs_to_path(
        input,
        cwd,
        user_workflow_dir,
        Some(config_dir),
        &persisted_path,
    )
}

pub fn resolve_workflow_script_with_user_dir_to_path(
    input: &WorkflowInput,
    cwd: &Path,
    user_workflow_dir: &Path,
    persisted_path: &Path,
) -> io::Result<ResolvedWorkflowScript> {
    let config_dir = orca_core::config::folder_trust::config_dir();
    resolve_workflow_script_with_dirs_to_path(
        input,
        cwd,
        user_workflow_dir,
        config_dir.as_deref(),
        persisted_path,
    )
}

fn resolve_workflow_script_with_dirs_to_path(
    input: &WorkflowInput,
    cwd: &Path,
    user_workflow_dir: &Path,
    config_dir: Option<&Path>,
    persisted_path: &Path,
) -> io::Result<ResolvedWorkflowScript> {
    let (source_kind, original_path, script) = if let Some(script_path) = input
        .script_path
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        let path = resolve_path(cwd, script_path);
        let script = fs::read_to_string(&path)?;
        (WorkflowScriptSource::ScriptPath, Some(path), script)
    } else if let Some(script) = input.script.as_ref() {
        (WorkflowScriptSource::InlineScript, None, script.clone())
    } else if let Some(name) = input
        .name
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        let path = find_named_workflow(cwd, name, user_workflow_dir, config_dir)?;
        let script = fs::read_to_string(&path)?;
        (WorkflowScriptSource::NamedWorkflow, Some(path), script)
    } else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "workflow input must include scriptPath, script, or name",
        ));
    };

    let meta = parse_workflow_meta(&script)?;
    validate_workflow_runtime_contract(&script, &meta)?;
    let args_schema = parse_workflow_args_schema(&script)?.unwrap_or_default();
    if let Some(parent) = persisted_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&persisted_path, &script)?;

    Ok(ResolvedWorkflowScript {
        source_kind,
        original_path,
        persisted_path: persisted_path.to_path_buf(),
        script_digest: sha256_hex(script.as_bytes()),
        meta,
        args_schema,
        script,
    })
}

pub fn contains_workflow_keyword(prompt: &str, config: &WorkflowConfig) -> bool {
    config.keyword_trigger_enabled && prompt.split_whitespace().any(|word| word == "ultracode")
}

fn resolve_path(cwd: &Path, raw_path: &str) -> PathBuf {
    let path = PathBuf::from(raw_path);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

fn find_named_workflow(
    cwd: &Path,
    name: &str,
    user_workflow_dir: &Path,
    config_dir: Option<&Path>,
) -> io::Result<PathBuf> {
    find_saved_workflow_in(cwd, name, user_workflow_dir, config_dir)
}

pub fn find_saved_workflow(
    cwd: &Path,
    name: &str,
    user_workflow_dir: &Path,
) -> io::Result<PathBuf> {
    let config_dir = orca_core::config::folder_trust::config_dir();
    find_saved_workflow_in(cwd, name, user_workflow_dir, config_dir.as_deref())
}

pub fn find_saved_workflow_with_config_dir(
    cwd: &Path,
    name: &str,
    user_workflow_dir: &Path,
    config_dir: &Path,
) -> io::Result<PathBuf> {
    find_saved_workflow_in(cwd, name, user_workflow_dir, Some(config_dir))
}

fn find_saved_workflow_in(
    cwd: &Path,
    name: &str,
    user_workflow_dir: &Path,
    config_dir: Option<&Path>,
) -> io::Result<PathBuf> {
    let mut untrusted: Option<PathBuf> = None;
    for ancestor in cwd.ancestors() {
        let candidate = ancestor
            .join(".orca")
            .join("workflows")
            .join(format!("{name}.js"));
        if !candidate.exists() {
            continue;
        }
        if config_dir.is_some_and(|config_dir| {
            orca_core::config::folder_trust::is_trusted_with_config_dir(ancestor, config_dir)
        }) {
            return Ok(candidate);
        }
        // Remember it: a workflow that exists but is gated is a trust problem, not a typo.
        untrusted.get_or_insert_with(|| candidate.clone());
    }

    let user_candidate = user_workflow_dir.join(format!("{name}.js"));
    if user_candidate.exists() {
        return Ok(user_candidate);
    }

    if let Some(path) = untrusted {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "workflow script `{name}` exists at {} but that workspace is not trusted \
                 (run `orca trust add --cwd {}`)",
                path.display(),
                path.parent()
                    .and_then(|workflows| workflows.parent())
                    .map(|workspace| workspace.display().to_string())
                    .unwrap_or_else(|| path.display().to_string())
            ),
        ));
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("workflow script `{name}` not found"),
    ))
}

pub fn parse_workflow_meta(script: &str) -> io::Result<WorkflowMeta> {
    validate_supported_workflow_exports(script)?;
    let export_index = script
        .find("export const meta")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing `export const meta`"))?;
    let object_start = script[export_index..]
        .find('{')
        .map(|offset| export_index + offset)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing meta object"))?;
    let object_end = find_matching_brace(script, object_start)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "unterminated meta object"))?;
    let body = &script[object_start + 1..object_end];

    let mut name = None;
    let mut description = None;
    let mut phases = None;
    let mut tags = Vec::new();
    let mut version = None;

    for field in split_top_level(body, ',') {
        let trimmed = field.trim();
        if trimmed.is_empty() {
            continue;
        }

        let Some((key, value)) = split_key_value(trimmed) else {
            continue;
        };
        match key.trim() {
            "name" => name = Some(parse_quoted_string(value)?),
            "description" => description = Some(parse_quoted_string(value)?),
            "phases" => phases = Some(parse_phases(value)?),
            "tags" => tags = parse_string_array(value)?,
            "version" => version = Some(parse_quoted_string(value)?),
            _ => {}
        }
    }

    if phases.is_none() {
        phases = parse_exported_phases(script)?;
    }

    Ok(WorkflowMeta {
        name: name.ok_or_else(|| missing_meta_field("name"))?,
        description: description.ok_or_else(|| missing_meta_field("description"))?,
        phases: phases.ok_or_else(|| missing_meta_field("phases"))?,
        tags,
        version,
    })
}

pub fn validate_workflow_runtime_contract(script: &str, meta: &WorkflowMeta) -> io::Result<()> {
    reject_unsupported_workflow_apis(script)?;

    if workflow_script_has_executable_marker(script)? {
        return Ok(());
    }

    if !meta.phases.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "hand-written workflow with string phases must call phase()/agent() at top level and export a default result, or use auto mode tasks: [{ prompt: \"...\" }]",
        ));
    }

    Ok(())
}

fn reject_unsupported_workflow_apis(script: &str) -> io::Result<()> {
    for marker in ["phase.agent(", ".runParallel("] {
        if script_contains_code_marker(script, marker)? {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "unsupported workflow API `{marker}`; use top-level phase(\"name\", async () => agent(\"prompt\")) or auto mode tasks: [{{ prompt: \"...\" }}]"
                ),
            ));
        }
    }
    Ok(())
}

fn workflow_script_has_executable_marker(script: &str) -> io::Result<bool> {
    for marker in [
        "export default",
        "agent(",
        "phase(",
        "parallel(",
        "pipeline(",
        "tasks",
    ] {
        if script_contains_code_marker(script, marker)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn script_contains_code_marker(script: &str, marker: &str) -> io::Result<bool> {
    let mut index = 0usize;
    while index < script.len() {
        let rest = &script[index..];
        if rest.starts_with("//") {
            index = skip_line_comment(script, index + 2);
            continue;
        }
        if rest.starts_with("/*") {
            index = skip_block_comment(script, index + 2)?;
            continue;
        }

        let Some(ch) = rest.chars().next() else {
            break;
        };
        if ch == '\'' || ch == '"' || ch == '`' {
            index = skip_quoted_or_template(script, index, ch)?;
            continue;
        }

        if rest.starts_with(marker) && marker_has_identifier_boundaries(script, index, marker) {
            return Ok(true);
        }

        index += ch.len_utf8();
    }

    Ok(false)
}

fn marker_has_identifier_boundaries(script: &str, start: usize, marker: &str) -> bool {
    let end = start + marker.len();
    let starts_with_identifier = marker.chars().next().is_some_and(is_identifier_part);
    let ends_with_identifier = marker.chars().last().is_some_and(is_identifier_part);
    let before = script[..start].chars().next_back();
    let after = script[end..].chars().next();

    (!starts_with_identifier || before.is_none_or(|ch| !is_identifier_part(ch)))
        && (!ends_with_identifier || after.is_none_or(|ch| !is_identifier_part(ch)))
}

pub fn parse_workflow_args_schema(script: &str) -> io::Result<Option<WorkflowArgsSchema>> {
    let Some(export_index) = script.find("export const args") else {
        return Ok(None);
    };
    let object_start = script[export_index..]
        .find('{')
        .map(|offset| export_index + offset)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing args object"))?;
    let object_end = find_matching_brace(script, object_start)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "unterminated args object"))?;
    let body = &script[object_start + 1..object_end];

    let mut schema = WorkflowArgsSchema::new();
    for field in split_top_level(body, ',') {
        let trimmed = field.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some((key, value)) = split_key_value(trimmed) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "workflow args field must use key: { ... }",
            ));
        };
        let key = parse_object_key(key)?;
        let spec = parse_workflow_arg_spec(value)?;
        schema.insert(key, spec);
    }

    Ok(Some(schema))
}

pub fn validate_workflow_args(
    args: Option<Value>,
    schema: &WorkflowArgsSchema,
) -> io::Result<Value> {
    if schema.is_empty() {
        return Ok(args.unwrap_or(Value::Null));
    }

    let mut object = match args.unwrap_or_else(|| Value::Object(Map::new())) {
        Value::Object(object) => object,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "workflow args must be an object when the script exports an args schema",
            ));
        }
    };

    for (name, spec) in schema {
        match object.get(name) {
            Some(value) if !arg_value_matches_type(value, spec.arg_type) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("workflow arg `{name}` must be {}", spec.arg_type.as_str()),
                ));
            }
            Some(_) => {}
            None => {
                if let Some(default) = spec.default.clone() {
                    object.insert(name.clone(), default);
                } else if spec.required {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("missing required workflow arg `{name}`"),
                    ));
                }
            }
        }
    }

    Ok(Value::Object(object))
}

fn parse_workflow_arg_spec(input: &str) -> io::Result<WorkflowArgSpec> {
    let trimmed = input.trim();
    if !trimmed.starts_with('{') || !trimmed.ends_with('}') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "workflow args spec must be an object",
        ));
    }
    let body = &trimmed[1..trimmed.len() - 1];
    let mut arg_type = None;
    let mut required = false;
    let mut default = None;

    for field in split_top_level(body, ',') {
        let trimmed = field.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some((key, value)) = split_key_value(trimmed) else {
            continue;
        };
        match parse_object_key(key)?.as_str() {
            "type" => arg_type = Some(parse_arg_type(value)?),
            "required" => required = parse_bool_literal(value)?,
            "default" => default = Some(parse_jsonish_literal(value)?),
            _ => {}
        }
    }

    Ok(WorkflowArgSpec {
        arg_type: arg_type.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "workflow args spec missing `type`",
            )
        })?,
        required,
        default,
    })
}

fn parse_object_key(input: &str) -> io::Result<String> {
    let trimmed = input.trim();
    if trimmed.starts_with('\'') || trimmed.starts_with('"') {
        return parse_quoted_string(trimmed);
    }
    if trimmed
        .chars()
        .all(|ch| ch == '_' || ch == '-' || ch.is_ascii_alphanumeric())
    {
        return Ok(trimmed.to_string());
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        format!("invalid workflow object key `{trimmed}`"),
    ))
}

fn parse_arg_type(input: &str) -> io::Result<WorkflowArgType> {
    match parse_quoted_string(input)?.as_str() {
        "string" => Ok(WorkflowArgType::String),
        "number" => Ok(WorkflowArgType::Number),
        "boolean" => Ok(WorkflowArgType::Boolean),
        "json" => Ok(WorkflowArgType::Json),
        value => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported workflow arg type `{value}`"),
        )),
    }
}

fn parse_bool_literal(input: &str) -> io::Result<bool> {
    match input.trim() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected boolean literal",
        )),
    }
}

fn parse_jsonish_literal(input: &str) -> io::Result<Value> {
    let trimmed = input.trim();
    if trimmed.starts_with('\'') || trimmed.starts_with('"') {
        return parse_quoted_string(trimmed).map(Value::String);
    }
    match trimmed {
        "true" => return Ok(Value::Bool(true)),
        "false" => return Ok(Value::Bool(false)),
        "null" => return Ok(Value::Null),
        _ => {}
    }
    if let Ok(number) = trimmed.parse::<i64>() {
        return Ok(Value::Number(Number::from(number)));
    }
    if let Ok(number) = trimmed.parse::<f64>() {
        if let Some(number) = Number::from_f64(number) {
            return Ok(Value::Number(number));
        }
    }
    serde_json::from_str(trimmed).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn arg_value_matches_type(value: &Value, arg_type: WorkflowArgType) -> bool {
    match arg_type {
        WorkflowArgType::String => value.is_string(),
        WorkflowArgType::Number => value.is_number(),
        WorkflowArgType::Boolean => value.is_boolean(),
        WorkflowArgType::Json => true,
    }
}

fn validate_supported_workflow_exports(script: &str) -> io::Result<()> {
    let mut index = 0usize;

    while index < script.len() {
        let rest = &script[index..];
        if rest.starts_with("//") {
            index = skip_line_comment(script, index + 2);
            continue;
        }
        if rest.starts_with("/*") {
            index = skip_block_comment(script, index + 2)?;
            continue;
        }

        let Some(ch) = rest.chars().next() else {
            break;
        };
        if ch == '\'' || ch == '"' || ch == '`' {
            index = skip_quoted_or_template(script, index, ch)?;
            continue;
        }

        if is_identifier_start(ch) {
            let ident_start = index;
            let ident_end = read_identifier_end(script, index + ch.len_utf8());
            if &script[ident_start..ident_end] == "export" {
                validate_workflow_export(script, ident_start, ident_end)?;
            }
            index = ident_end;
            continue;
        }

        index += ch.len_utf8();
    }

    Ok(())
}

fn validate_workflow_export(
    script: &str,
    export_start: usize,
    export_end: usize,
) -> io::Result<()> {
    if has_identifier_neighbor(script, export_start, export_end) {
        return Ok(());
    }

    let first_start = skip_ignorable(script, export_end)?;
    let Some(first_char) = script[first_start..].chars().next() else {
        return unsupported_workflow_export("export");
    };
    if !is_identifier_start(first_char) {
        return unsupported_workflow_export("export");
    }
    let first_end = read_identifier_end(script, first_start + first_char.len_utf8());
    let first = &script[first_start..first_end];

    match first {
        "default" => Ok(()),
        "const" => {
            let second_start = skip_ignorable(script, first_end)?;
            let Some(second_char) = script[second_start..].chars().next() else {
                return unsupported_workflow_export("export const");
            };
            if !is_identifier_start(second_char) {
                return unsupported_workflow_export("export const");
            }
            let second_end = read_identifier_end(script, second_start + second_char.len_utf8());
            match &script[second_start..second_end] {
                "meta" | "phases" | "args" => Ok(()),
                other => unsupported_workflow_export(&format!("export const {other}")),
            }
        }
        other => unsupported_workflow_export(&format!("export {other}")),
    }
}

fn unsupported_workflow_export<T>(kind: &str) -> io::Result<T> {
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "unsupported workflow export `{kind}`; workflow scripts may export only `const meta`, `const phases`, `const args`, or `default`"
        ),
    ))
}

fn skip_ignorable(script: &str, mut index: usize) -> io::Result<usize> {
    while index < script.len() {
        let rest = &script[index..];
        if rest.starts_with("//") {
            index = skip_line_comment(script, index + 2);
            continue;
        }
        if rest.starts_with("/*") {
            index = skip_block_comment(script, index + 2)?;
            continue;
        }
        let Some(ch) = rest.chars().next() else {
            break;
        };
        if !ch.is_whitespace() {
            break;
        }
        index += ch.len_utf8();
    }
    Ok(index)
}

fn skip_line_comment(script: &str, mut index: usize) -> usize {
    while index < script.len() {
        let Some(ch) = script[index..].chars().next() else {
            break;
        };
        index += ch.len_utf8();
        if ch == '\n' {
            break;
        }
    }
    index
}

fn skip_block_comment(script: &str, mut index: usize) -> io::Result<usize> {
    while index + 1 < script.len() {
        if script[index..].starts_with("*/") {
            return Ok(index + 2);
        }
        let Some(ch) = script[index..].chars().next() else {
            break;
        };
        index += ch.len_utf8();
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "unterminated block comment",
    ))
}

fn skip_quoted_or_template(script: &str, start: usize, quote: char) -> io::Result<usize> {
    let mut index = start + quote.len_utf8();
    let mut escaped = false;
    while index < script.len() {
        let Some(ch) = script[index..].chars().next() else {
            break;
        };
        index += ch.len_utf8();
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == quote {
            return Ok(index);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "unterminated string literal",
    ))
}

fn read_identifier_end(script: &str, mut index: usize) -> usize {
    while index < script.len() {
        let Some(ch) = script[index..].chars().next() else {
            break;
        };
        if !is_identifier_part(ch) {
            break;
        }
        index += ch.len_utf8();
    }
    index
}

fn is_identifier_start(ch: char) -> bool {
    ch == '_' || ch == '$' || ch.is_ascii_alphabetic()
}

fn is_identifier_part(ch: char) -> bool {
    is_identifier_start(ch) || ch.is_ascii_digit()
}

fn has_identifier_neighbor(script: &str, start: usize, end: usize) -> bool {
    let before = script[..start].chars().next_back();
    let after = script[end..].chars().next();
    before.is_some_and(is_identifier_part) || after.is_some_and(is_identifier_part)
}

fn parse_exported_phases(script: &str) -> io::Result<Option<Vec<String>>> {
    let Some(export_index) = script.find("export const phases") else {
        return Ok(None);
    };
    let Some(equals_offset) = script[export_index..].find('=') else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "missing phases assignment",
        ));
    };
    let value_start = export_index + equals_offset + 1;
    let array_start = script[value_start..]
        .find('[')
        .map(|offset| value_start + offset)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing phases array"))?;
    let array_end = find_matching_bracket(script, array_start)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "unterminated phases array"))?;
    parse_phase_names(&script[array_start..=array_end]).map(Some)
}

fn find_matching_brace(script: &str, object_start: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut quote = None;
    let mut escaped = false;

    for (index, ch) in script[object_start..].char_indices() {
        let absolute = object_start + index;
        if let Some(active_quote) = quote {
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
                continue;
            }
            if ch == active_quote {
                quote = None;
            }
            continue;
        }

        match ch {
            '\'' | '"' => quote = Some(ch),
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(absolute);
                }
            }
            _ => {}
        }
    }

    None
}

fn find_matching_bracket(script: &str, array_start: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut quote = None;
    let mut escaped = false;

    for (index, ch) in script[array_start..].char_indices() {
        let absolute = array_start + index;
        if let Some(active_quote) = quote {
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
                continue;
            }
            if ch == active_quote {
                quote = None;
            }
            continue;
        }

        match ch {
            '\'' | '"' => quote = Some(ch),
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(absolute);
                }
            }
            _ => {}
        }
    }

    None
}

fn split_top_level(input: &str, delimiter: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut quote = None;
    let mut escaped = false;

    for (index, ch) in input.char_indices() {
        if let Some(active_quote) = quote {
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
                continue;
            }
            if ch == active_quote {
                quote = None;
            }
            continue;
        }

        match ch {
            '\'' | '"' => quote = Some(ch),
            '[' => bracket_depth += 1,
            ']' => bracket_depth = bracket_depth.saturating_sub(1),
            '{' => brace_depth += 1,
            '}' => brace_depth = brace_depth.saturating_sub(1),
            _ if ch == delimiter && bracket_depth == 0 && brace_depth == 0 => {
                parts.push(&input[start..index]);
                start = index + ch.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(&input[start..]);
    parts
}

fn split_key_value(input: &str) -> Option<(&str, &str)> {
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut quote = None;
    let mut escaped = false;

    for (index, ch) in input.char_indices() {
        if let Some(active_quote) = quote {
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
                continue;
            }
            if ch == active_quote {
                quote = None;
            }
            continue;
        }

        match ch {
            '\'' | '"' => quote = Some(ch),
            '[' => bracket_depth += 1,
            ']' => bracket_depth = bracket_depth.saturating_sub(1),
            '{' => brace_depth += 1,
            '}' => brace_depth = brace_depth.saturating_sub(1),
            ':' if bracket_depth == 0 && brace_depth == 0 => {
                return Some((&input[..index], &input[index + 1..]));
            }
            _ => {}
        }
    }

    None
}

fn parse_quoted_string(input: &str) -> io::Result<String> {
    let trimmed = input.trim();
    if trimmed.len() < 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected quoted string",
        ));
    }

    let quote = trimmed
        .chars()
        .next()
        .filter(|ch| *ch == '\'' || *ch == '"')
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "expected quoted string"))?;
    if !trimmed.ends_with(quote) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unterminated quoted string",
        ));
    }

    Ok(trimmed[1..trimmed.len() - 1].to_string())
}

fn parse_phases(input: &str) -> io::Result<Vec<String>> {
    parse_phase_names(input)
}

fn parse_string_array(input: &str) -> io::Result<Vec<String>> {
    let trimmed = input.trim();
    if !trimmed.starts_with('[') || !trimmed.ends_with(']') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected string array",
        ));
    }

    let body = &trimmed[1..trimmed.len() - 1];
    if body.trim().is_empty() {
        return Ok(Vec::new());
    }

    split_top_level(body, ',')
        .into_iter()
        .map(parse_quoted_string)
        .collect()
}

fn parse_phase_names(input: &str) -> io::Result<Vec<String>> {
    let trimmed = input.trim();
    if !trimmed.starts_with('[') || !trimmed.ends_with(']') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected phases array",
        ));
    }

    let body = &trimmed[1..trimmed.len() - 1];
    if body.trim().is_empty() {
        return Ok(Vec::new());
    }

    split_top_level(body, ',')
        .into_iter()
        .map(parse_phase_name)
        .collect()
}

fn parse_phase_name(input: &str) -> io::Result<String> {
    let trimmed = input.trim();
    if trimmed.starts_with('\'') || trimmed.starts_with('"') {
        return parse_quoted_string(trimmed);
    }
    if !trimmed.starts_with('{') || !trimmed.ends_with('}') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected quoted string or phase object",
        ));
    }

    let body = &trimmed[1..trimmed.len() - 1];
    let mut name = None;
    for field in split_top_level(body, ',') {
        let Some((key, value)) = split_key_value(field.trim()) else {
            continue;
        };
        match key.trim() {
            "name" => name = Some(parse_quoted_string(value)?),
            "tasks" => validate_phase_tasks(value)?,
            _ => {}
        }
    }

    name.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "workflow phase object missing `name`",
        )
    })
}

fn validate_phase_tasks(input: &str) -> io::Result<()> {
    let trimmed = input.trim();
    if !trimmed.starts_with('[') || !trimmed.ends_with(']') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "workflow phase `tasks` must be an array",
        ));
    }

    let body = &trimmed[1..trimmed.len() - 1];
    if body.trim().is_empty() {
        return Ok(());
    }

    for task in split_top_level(body, ',') {
        validate_phase_task(task.trim())?;
    }

    Ok(())
}

fn validate_phase_task(input: &str) -> io::Result<()> {
    if !input.starts_with('{') || !input.ends_with('}') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "workflow phase task must be an object with `prompt`",
        ));
    }

    let body = &input[1..input.len() - 1];
    for field in split_top_level(body, ',') {
        let Some((key, value)) = split_key_value(field.trim()) else {
            continue;
        };
        if key.trim() == "prompt" {
            let prompt = parse_quoted_string(value)?;
            if prompt.trim().is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "workflow phase task `prompt` cannot be empty",
                ));
            }
            return Ok(());
        }
    }

    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "workflow phase task must be an object with `prompt`",
    ))
}

fn missing_meta_field(field: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("workflow meta missing `{field}`"),
    )
}

fn sha256_hex(input: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input);
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use orca_core::config::WorkflowConfig;

    use super::{
        contains_workflow_keyword, find_saved_workflow_with_config_dir, parse_workflow_meta,
    };

    #[test]
    fn parser_accepts_double_quotes() {
        let meta = parse_workflow_meta(
            "export const meta = { name: \"audit\", description: \"Audit code\", phases: [] };",
        )
        .unwrap();
        assert_eq!(meta.name, "audit");
        assert!(meta.phases.is_empty());
    }

    #[test]
    fn parser_accepts_phases_exported_separately_from_meta() {
        let meta = parse_workflow_meta(
            "export const meta = { name: \"audit\", description: \"Audit code\" };\nexport const phases = [\"scan\", \"review\"];",
        )
        .unwrap();
        assert_eq!(meta.name, "audit");
        assert_eq!(meta.phases, vec!["scan", "review"]);
    }

    #[test]
    fn parser_rejects_unsupported_workflow_export() {
        let error = parse_workflow_meta(
            "export const meta = { name: \"audit\", description: \"Audit code\", phases: [\"scan\"] };\nexport async function run() {}",
        )
        .expect_err("unsupported export should be rejected before launch");

        assert!(
            error.to_string().contains("unsupported workflow export"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn parser_rejects_phase_task_strings_in_auto_mode() {
        let error = parse_workflow_meta(
            "export const meta = { name: \"audit\", description: \"Audit code\", phases: [{ name: \"scan\", tasks: [\"inspect\"] }] };",
        )
        .expect_err("auto workflow tasks must be prompt objects");

        assert!(
            error
                .to_string()
                .contains("workflow phase task must be an object with `prompt`"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn workflow_keyword_requires_exact_word_and_enabled_switch() {
        let enabled = WorkflowConfig::default();
        assert!(contains_workflow_keyword(
            "please run ultracode now",
            &enabled
        ));
        assert!(!contains_workflow_keyword(
            "please run ultracode-now",
            &enabled
        ));

        let disabled = WorkflowConfig {
            keyword_trigger_enabled: false,
            ..WorkflowConfig::default()
        };
        assert!(!contains_workflow_keyword(
            "please run ultracode now",
            &disabled
        ));
    }

    #[test]
    fn named_project_workflows_require_folder_trust() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let project_workflows = project.path().join(".orca/workflows");
        let user_workflows = home.path().join("workflows");
        std::fs::create_dir_all(&project_workflows).unwrap();
        std::fs::create_dir_all(&user_workflows).unwrap();
        let project_script = project_workflows.join("audit.js");
        let user_script = user_workflows.join("audit.js");
        std::fs::write(&project_script, "project").unwrap();
        std::fs::write(&user_script, "user").unwrap();

        assert_eq!(
            find_saved_workflow_with_config_dir(
                project.path(),
                "audit",
                &user_workflows,
                home.path(),
            )
            .unwrap(),
            user_script
        );

        orca_core::config::folder_trust::set_trust_with_config_dir(
            project.path(),
            home.path(),
            orca_core::config::folder_trust::TrustLevel::Trusted,
        )
        .unwrap();
        assert_eq!(
            find_saved_workflow_with_config_dir(
                project.path(),
                "audit",
                &user_workflows,
                home.path(),
            )
            .unwrap(),
            project_script
        );
    }

    #[test]
    fn untrusted_project_workflow_explains_the_trust_gate() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let project_workflows = project.path().join(".orca/workflows");
        std::fs::create_dir_all(&project_workflows).unwrap();
        let project_script = project_workflows.join("audit.js");
        std::fs::write(&project_script, "project").unwrap();
        // No user-level workflow: the project one is the only candidate and it is gated.
        let user_workflows = home.path().join("workflows");

        let error = find_saved_workflow_with_config_dir(
            project.path(),
            "audit",
            &user_workflows,
            home.path(),
        )
        .expect_err("an untrusted project workflow must not resolve");
        let message = error.to_string();
        assert!(
            message.contains("is not trusted") && message.contains("orca trust add"),
            "unhelpful error: {message}"
        );
        assert!(
            message.contains(&project.path().display().to_string()),
            "error should name the workspace: {message}"
        );
    }
}
