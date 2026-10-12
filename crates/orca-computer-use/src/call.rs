use serde_json::Value;

use crate::keys;
use crate::protocol::{Action, Button, Direction, Target};

pub const DEFAULT_MAX_ELEMENTS: u32 = 300;
pub const MAX_MAX_ELEMENTS: u32 = 1000;

#[derive(Clone, Debug, PartialEq)]
pub enum ComputerCall {
    ListApps,
    LaunchApp {
        app: String,
        open: Vec<String>,
    },
    Observe {
        app: String,
        window: Option<u32>,
        query: Option<String>,
        max_elements: u32,
    },
    Act {
        app: String,
        window: Option<u32>,
        action: Action,
        observe_after: bool,
    },
}

impl ComputerCall {
    pub fn parse(arguments: &Value) -> Result<ComputerCall, String> {
        if !arguments.is_object() {
            return Err("computer arguments must be an object".into());
        }
        let name = required_string(arguments, "action", "computer")?;
        validate_action_fields(arguments, name)?;
        if name == "list_apps" {
            return Ok(Self::ListApps);
        }
        let app = required_string(arguments, "app", name)?.to_owned();
        if name == "launch_app" {
            let open = string_list(arguments, "open", name, false)?;
            return Ok(Self::LaunchApp { app, open });
        }
        let window = optional_u32(arguments, "window")?;
        if name == "observe" {
            let query = optional_string(arguments, "query")?;
            let max_elements = match arguments.get("max_elements") {
                None => DEFAULT_MAX_ELEMENTS,
                Some(value) => value
                    .as_u64()
                    .ok_or_else(|| "\"max_elements\" must be a non-negative integer".to_owned())?
                    .min(u64::from(MAX_MAX_ELEMENTS)) as u32,
            };
            return Ok(Self::Observe {
                app,
                window,
                query,
                max_elements,
            });
        }
        let observe_after = match arguments.get("observe") {
            None => true,
            Some(value) => value
                .as_bool()
                .ok_or_else(|| "\"observe\" must be a boolean".to_owned())?,
        };
        let action = match name {
            "click" => {
                let target = required_target(arguments, name)?;
                let button = match arguments.get("button") {
                    None => Button::Left,
                    Some(value) => serde_json::from_value(value.clone())
                        .map_err(|_| "\"button\" must be \"left\" or \"right\"".to_owned())?,
                };
                let count = match arguments.get("count") {
                    None => 1,
                    Some(value) => match value.as_u64() {
                        Some(1) => 1,
                        Some(2) => 2,
                        _ => return Err("\"count\" must be 1 or 2".into()),
                    },
                };
                let modifiers = string_list(arguments, "modifiers", name, false)?;
                for modifier in &modifiers {
                    keys::parse_key(modifier)?;
                    if !matches!(
                        modifier.as_str(),
                        "cmd" | "shift" | "option" | "alt" | "ctrl"
                    ) {
                        return Err(format!(
                            "\"modifiers\" contains a non-modifier key {modifier:?}"
                        ));
                    }
                }
                Action::Click {
                    target,
                    button,
                    count,
                    modifiers,
                }
            }
            "type" => {
                let text = required_string(arguments, "text", name)?;
                crate::type_text::validate_controls(text)?;
                Action::Type {
                    text: text.to_owned(),
                    target: optional_target(arguments, name)?,
                }
            }
            "key" => {
                let keys = string_list(arguments, "keys", name, true)?;
                keys::parse_combo(&keys)?;
                Action::Key {
                    keys,
                    target: optional_target(arguments, name)?,
                }
            }
            "scroll" => {
                let target = required_target(arguments, name)?;
                let direction: Direction = serde_json::from_value(
                    arguments
                        .get("direction")
                        .cloned()
                        .ok_or_else(|| "scroll needs \"direction\"".to_owned())?,
                )
                .map_err(|_| {
                    "\"direction\" must be \"up\", \"down\", \"left\" or \"right\"".to_owned()
                })?;
                let amount = match arguments.get("amount") {
                    None => 3,
                    Some(value) => parse_u32(value, "amount")?,
                };
                Action::Scroll {
                    target,
                    direction,
                    amount,
                }
            }
            _ => unreachable!("action name was validated"),
        };
        Ok(Self::Act {
            app,
            window,
            action,
            observe_after,
        })
    }

    pub fn app(&self) -> Option<&str> {
        match self {
            Self::ListApps => None,
            Self::LaunchApp { app, .. } | Self::Observe { app, .. } | Self::Act { app, .. } => {
                Some(app)
            }
        }
    }

    pub fn action_name(&self) -> &'static str {
        match self {
            Self::ListApps => "list_apps",
            Self::LaunchApp { .. } => "launch_app",
            Self::Observe { .. } => "observe",
            Self::Act { action, .. } => match action {
                Action::Click { .. } => "click",
                Action::Type { .. } => "type",
                Action::Key { .. } => "key",
                Action::Scroll { .. } => "scroll",
            },
        }
    }
}

fn action_fields(action: &str) -> Result<&'static [&'static str], String> {
    let allowed: &[&str] = match action {
        "list_apps" => &["action"],
        "launch_app" => &["action", "app", "open"],
        "observe" => &["action", "app", "window", "query", "max_elements"],
        "click" => &[
            "action",
            "app",
            "window",
            "target",
            "button",
            "count",
            "modifiers",
            "observe",
        ],
        "type" => &["action", "app", "window", "target", "text", "observe"],
        "key" => &["action", "app", "window", "target", "keys", "observe"],
        "scroll" => &[
            "action",
            "app",
            "window",
            "target",
            "direction",
            "amount",
            "observe",
        ],
        _ => return Err(format!("unknown computer action {action:?}")),
    };
    Ok(allowed)
}

#[cfg(feature = "test-utils")]
pub fn action_fields_for_test(action: &str) -> Option<&'static [&'static str]> {
    action_fields(action).ok()
}

fn validate_action_fields(arguments: &Value, action: &str) -> Result<(), String> {
    let allowed = action_fields(action)?;
    if action == "key" && arguments.get("modifiers").is_some() {
        return Err(
            "`key` takes its modifiers inside `keys`, for example `[\"cmd\",\"shift\",\"t\"]`"
                .into(),
        );
    }
    for field in arguments
        .as_object()
        .expect("arguments were checked")
        .keys()
    {
        if allowed.contains(&field.as_str()) {
            continue;
        }
        return Err(format!(
            "{action} does not accept {field:?}; remove it. Allowed fields: {}",
            allowed.join(", ")
        ));
    }
    Ok(())
}

fn required_string<'a>(arguments: &'a Value, field: &str, action: &str) -> Result<&'a str, String> {
    arguments
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{action} needs \"{field}\" as a string"))
}

fn optional_string(arguments: &Value, field: &str) -> Result<Option<String>, String> {
    match arguments.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_str()
            .map(|value| Some(value.to_owned()))
            .ok_or_else(|| format!("\"{field}\" must be a string")),
    }
}

fn parse_u32(value: &Value, field: &str) -> Result<u32, String> {
    value
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| format!("\"{field}\" must be an integer from 0 to {}", u32::MAX))
}

fn optional_u32(arguments: &Value, field: &str) -> Result<Option<u32>, String> {
    match arguments.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => parse_u32(value, field).map(Some),
    }
}

fn string_list(
    arguments: &Value,
    field: &str,
    action: &str,
    required: bool,
) -> Result<Vec<String>, String> {
    match arguments.get(field) {
        None if !required => Ok(vec![]),
        Some(Value::Array(values)) => values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| format!("\"{field}\" must be an array of strings"))
            })
            .collect(),
        _ => Err(format!("{action} needs \"{field}\" as an array of strings")),
    }
}

fn target_error(action: &str) -> String {
    format!("{action} needs \"target\": {{\"element\": N}} or {{\"point\": [x, y]}}")
}

fn required_target(arguments: &Value, action: &str) -> Result<Target, String> {
    let value = arguments
        .get("target")
        .ok_or_else(|| target_error(action))?;
    if value.as_object().is_none_or(|target| target.len() != 1) {
        return Err(target_error(action));
    }
    serde_json::from_value(value.clone()).map_err(|_| target_error(action))
}

fn optional_target(arguments: &Value, action: &str) -> Result<Option<Target>, String> {
    match arguments.get("target") {
        None | Some(Value::Null) => Ok(None),
        Some(_) => required_target(arguments, action).map(Some),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn type_refuses_non_text_controls_before_delivery() {
        for control in (0u8..=31)
            .filter(|c| !matches!(c, 9 | 10 | 13))
            .chain([127])
        {
            let result = super::ComputerCall::parse(&serde_json::json!({
                "action":"type", "app":"x", "text":format!("before{}after", char::from(control))
            }));
            assert!(result.is_err(), "control U+{control:04X} must point to key");
            assert!(result.unwrap_err().contains("key"));
        }
    }

    use super::*;
    use serde_json::json;

    fn rejects_unrelated_fields(arguments: Value, allowed: &[&str]) {
        assert!(
            ComputerCall::parse(&arguments).is_ok(),
            "valid control: {arguments}"
        );
        // Literal schema fields, independent of the parser's action allowlist.
        for field in [
            "app",
            "window",
            "open",
            "query",
            "max_elements",
            "target",
            "button",
            "count",
            "modifiers",
            "text",
            "keys",
            "direction",
            "amount",
            "observe",
            "unexpected",
        ] {
            if allowed.contains(&field) {
                continue;
            }
            let mut invalid = arguments.clone();
            invalid[field] = Value::Null;
            let error = ComputerCall::parse(&invalid).expect_err(&format!(
                "{} silently accepted {field}",
                arguments["action"]
            ));
            assert!(
                error.contains(field),
                "must identify the rejected field: {error}"
            );
            assert!(
                error.contains("remove") || error.contains("inside `keys`"),
                "must explain how to correct the call: {error}"
            );
        }
    }

    #[test]
    fn list_apps_rejects_fields_from_other_actions() {
        rejects_unrelated_fields(json!({"action":"list_apps"}), &[]);
    }

    #[test]
    fn launch_app_rejects_fields_from_other_actions() {
        rejects_unrelated_fields(
            json!({"action":"launch_app", "app":"TextEdit", "open":["/tmp/probe.txt"]}),
            &["app", "open"],
        );
    }

    #[test]
    fn observe_rejects_fields_from_other_actions() {
        rejects_unrelated_fields(
            json!({"action":"observe", "app":"x", "window":7, "query":"Save", "max_elements":100}),
            &["app", "window", "query", "max_elements"],
        );
    }

    #[test]
    fn click_rejects_fields_from_other_actions() {
        rejects_unrelated_fields(
            json!({"action":"click", "app":"x", "window":7, "target":{"element":1}, "button":"right", "count":2, "modifiers":["cmd"], "observe":false}),
            &[
                "app",
                "window",
                "target",
                "button",
                "count",
                "modifiers",
                "observe",
            ],
        );
    }

    #[test]
    fn type_rejects_fields_from_other_actions() {
        rejects_unrelated_fields(
            json!({"action":"type", "app":"x", "window":7, "target":{"element":1}, "text":"hello", "observe":false}),
            &["app", "window", "target", "text", "observe"],
        );
    }

    #[test]
    fn key_rejects_fields_from_other_actions() {
        rejects_unrelated_fields(
            json!({"action":"key", "app":"x", "window":7, "target":{"element":1}, "keys":["cmd","s"], "observe":false}),
            &["app", "window", "target", "keys", "observe"],
        );
    }

    #[test]
    fn scroll_rejects_fields_from_other_actions() {
        rejects_unrelated_fields(
            json!({"action":"scroll", "app":"x", "window":7, "target":{"element":1}, "direction":"down", "amount":4, "observe":false}),
            &["app", "window", "target", "direction", "amount", "observe"],
        );
    }

    #[test]
    fn key_modifiers_explain_the_correct_keys_array() {
        assert_eq!(ComputerCall::parse(&json!({"action":"key", "app":"TextEdit", "keys":["t"], "modifiers":["shift","cmd"]})).unwrap_err(), "`key` takes its modifiers inside `keys`, for example `[\"cmd\",\"shift\",\"t\"]`");
        assert!(
            ComputerCall::parse(
                &json!({"action":"key", "app":"TextEdit", "keys":["cmd","shift","t"]})
            )
            .is_ok()
        );
    }

    #[test]
    fn key_modifier_hint_wins_over_unrelated_fields_in_every_order() {
        for raw in [
            r#"{"action":"key","app":"x","button":"left","keys":["t"],"modifiers":["cmd","shift"]}"#,
            r#"{"modifiers":["cmd","shift"],"keys":["t"],"button":"left","app":"x","action":"key"}"#,
            r#"{"action":"key","amount":2,"keys":["t"],"modifiers":null}"#,
        ] {
            let arguments: Value = serde_json::from_str(raw).unwrap();
            assert_eq!(
                ComputerCall::parse(&arguments).unwrap_err(),
                "`key` takes its modifiers inside `keys`, for example `[\"cmd\",\"shift\",\"t\"]`"
            );
        }
    }

    #[test]
    fn click_defaults_to_one_left_click_and_observes_after() {
        let call = ComputerCall::parse(
            &json!({"action": "click", "app": "com.apple.TextEdit", "target": {"element": 12}}),
        )
        .unwrap();
        assert_eq!(
            call,
            ComputerCall::Act {
                app: "com.apple.TextEdit".into(),
                window: None,
                observe_after: true,
                action: Action::Click {
                    target: Target::Element { element: 12 },
                    button: Button::Left,
                    count: 1,
                    modifiers: vec![]
                }
            }
        );
    }

    #[test]
    fn a_click_without_a_target_says_what_is_missing() {
        assert_eq!(
            ComputerCall::parse(&json!({"action": "click", "app": "x"})).unwrap_err(),
            "click needs \"target\": {\"element\": N} or {\"point\": [x, y]}"
        );
    }

    #[test]
    fn list_launch_and_observe_have_defaults_and_accessors() {
        let list = ComputerCall::parse(&json!({"action":"list_apps"})).unwrap();
        assert_eq!(list, ComputerCall::ListApps);
        assert_eq!(list.app(), None);
        assert_eq!(list.action_name(), "list_apps");
        let launch =
            ComputerCall::parse(&json!({"action":"launch_app", "app":"TextEdit"})).unwrap();
        assert_eq!(
            launch,
            ComputerCall::LaunchApp {
                app: "TextEdit".into(),
                open: vec![]
            }
        );
        assert_eq!(launch.app(), Some("TextEdit"));
        assert_eq!(launch.action_name(), "launch_app");
        assert_eq!(
            ComputerCall::parse(
                &json!({"action":"launch_app", "app":"x", "open":["file", "https://example.com"]})
            )
            .unwrap(),
            ComputerCall::LaunchApp {
                app: "x".into(),
                open: vec!["file".into(), "https://example.com".into()]
            }
        );
        assert_eq!(DEFAULT_MAX_ELEMENTS, 300);
        assert_eq!(MAX_MAX_ELEMENTS, 1000);
        for (input, expected) in [
            (None, 300),
            (Some(42), 42),
            (Some(1001), 1000),
            (Some(u64::MAX), 1000),
        ] {
            let mut args = json!({"action":"observe", "app":"x", "window":9, "query":"Save"});
            if let Some(value) = input {
                args["max_elements"] = json!(value);
            }
            let call = ComputerCall::parse(&args).unwrap();
            assert_eq!(
                call,
                ComputerCall::Observe {
                    app: "x".into(),
                    window: Some(9),
                    query: Some("Save".into()),
                    max_elements: expected
                }
            );
            assert_eq!(call.action_name(), "observe");
        }
    }

    #[test]
    fn every_action_parses_its_options() {
        let cases = [
            (
                json!({"action":"click", "app":"x", "target":{"point":[1.5,2]}, "button":"right", "count":2, "modifiers":["alt", "shift"]}),
                Action::Click {
                    target: Target::Point { point: [1.5, 2.0] },
                    button: Button::Right,
                    count: 2,
                    modifiers: vec!["alt".into(), "shift".into()],
                },
                "click",
            ),
            (
                json!({"action":"type", "app":"x", "text":"你好"}),
                Action::Type {
                    text: "你好".into(),
                    target: None,
                },
                "type",
            ),
            (
                json!({"action":"key", "app":"x", "keys":["cmd","s"], "target":{"element":8}}),
                Action::Key {
                    keys: vec!["cmd".into(), "s".into()],
                    target: Some(Target::Element { element: 8 }),
                },
                "key",
            ),
            (
                json!({"action":"scroll", "app":"x", "target":{"element":8}, "direction":"down"}),
                Action::Scroll {
                    target: Target::Element { element: 8 },
                    direction: Direction::Down,
                    amount: 3,
                },
                "scroll",
            ),
            (
                json!({"action":"scroll", "app":"x", "target":{"point":[1,2]}, "direction":"left", "amount":7}),
                Action::Scroll {
                    target: Target::Point { point: [1.0, 2.0] },
                    direction: Direction::Left,
                    amount: 7,
                },
                "scroll",
            ),
            (
                json!({"action":"type", "app":"x", "text":"", "target":{"element":2}}),
                Action::Type {
                    text: "".into(),
                    target: Some(Target::Element { element: 2 }),
                },
                "type",
            ),
        ];
        for (mut args, action, name) in cases {
            args["observe"] = json!(false);
            args["window"] = json!(7);
            let call = ComputerCall::parse(&args).unwrap();
            assert_eq!(
                call,
                ComputerCall::Act {
                    app: "x".into(),
                    window: Some(7),
                    action,
                    observe_after: false
                }
            );
            assert_eq!(call.app(), Some("x"));
            assert_eq!(call.action_name(), name);
        }
    }

    #[test]
    fn missing_and_invalid_arguments_name_the_parameter() {
        let cases = [
            (json!([]), "object"),
            (json!({}), "action"),
            (json!({"action":"drag"}), "drag"),
            (json!({"action":"observe"}), "app"),
            (json!({"action":"observe", "app":5}), "app"),
            (
                json!({"action":"observe", "app":"x", "window":-1}),
                "window",
            ),
            (
                json!({"action":"observe", "app":"x", "window":4294967296u64}),
                "window",
            ),
            (json!({"action":"observe", "app":"x", "query":4}), "query"),
            (
                json!({"action":"observe", "app":"x", "max_elements":-1}),
                "max_elements",
            ),
            (
                json!({"action":"launch_app", "app":"x", "open":[1]}),
                "open",
            ),
            (json!({"action":"type", "app":"x"}), "text"),
            (json!({"action":"key", "app":"x"}), "keys"),
            (json!({"action":"key", "app":"x", "keys":[]}), "keys"),
            (
                json!({"action":"key", "app":"x", "keys":["unknown","s"]}),
                "unknown",
            ),
            (
                json!({"action":"key", "app":"x", "keys":["s","a"]}),
                "modifier",
            ),
            (
                json!({"action":"scroll", "app":"x", "target":{"element":2}}),
                "direction",
            ),
            (
                json!({"action":"scroll", "app":"x", "target":{"element":2}, "direction":"diagonal"}),
                "direction",
            ),
            (
                json!({"action":"scroll", "app":"x", "target":{"element":2}, "direction":"up", "amount":-1}),
                "amount",
            ),
            (
                json!({"action":"click", "app":"x", "target":{"element":2}, "button":"middle"}),
                "button",
            ),
            (
                json!({"action":"click", "app":"x", "target":{"element":2}, "modifiers":["s"]}),
                "modifiers",
            ),
            (
                json!({"action":"click", "app":"x", "target":{"element":2}, "observe":"no"}),
                "observe",
            ),
        ];
        for (args, parameter) in cases {
            let error = ComputerCall::parse(&args).unwrap_err();
            assert!(error.contains(parameter), "{args}: {error}");
        }
    }

    #[test]
    fn click_count_and_targets_are_validated() {
        for count in [json!(0), json!(3), json!(1.5), json!("1")] {
            let error = ComputerCall::parse(
                &json!({"action":"click", "app":"x", "target":{"element":1}, "count":count}),
            )
            .unwrap_err();
            assert!(error.contains("count"), "{error}");
        }
        for target in [
            json!({}),
            json!({"element":-1}),
            json!({"element":1,"point":[1,2]}),
            json!({"point":[1]}),
            json!({"point":[1,"x"]}),
            json!({"point":[1,2,3]}),
        ] {
            for action in ["click", "type", "key", "scroll"] {
                let error = ComputerCall::parse(&json!({"action":action,"app":"x","target":target,"text":"a","keys":["a"],"direction":"up"})).unwrap_err();
                assert!(error.contains("target"), "{error}");
            }
        }
    }
}
