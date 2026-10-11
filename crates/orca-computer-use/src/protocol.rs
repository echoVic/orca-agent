use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RequestLine {
    pub id: u64,
    #[serde(flatten)]
    pub request: Request,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum Request {
    Hello {
        protocol: u32,
    },
    ListApps,
    ResolveApp {
        app: String,
        allow_not_running: bool,
    },
    LaunchApp {
        app: String,
        open: Vec<String>,
    },
    Observe(ObserveParams),
    Act(ActParams),
    Turn {
        active: bool,
    },
    Shutdown,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ObserveParams {
    pub app: String, // Bundle ID
    pub window: Option<u32>,
    pub query: Option<String>,
    pub max_elements: u32,
    pub include_screenshot: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ActParams {
    pub app: String,
    pub window: Option<u32>,
    pub action: Action,
    pub foreground_fallback: bool,
    pub observe_after: bool,
    pub include_screenshot: bool,
    pub secure_approved: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    Click {
        target: Target,
        button: Button,
        count: u8,
        modifiers: Vec<String>,
    },
    Type {
        text: String,
        target: Option<Target>,
    },
    Key {
        keys: Vec<String>,
        target: Option<Target>,
    },
    Scroll {
        target: Target,
        direction: Direction,
        amount: u32,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Target {
    Element { element: u32 },
    Point { point: [f64; 2] },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Button {
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ResponseLine {
    pub id: u64,
    pub result: Result<Reply, DriverError>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct EventLine {
    pub event: DriverEvent,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DriverEvent {
    Esc,
}

/// One line the driver writes: a response, or an event at any time.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(untagged)]
pub enum DriverLine {
    Response(ResponseLine),
    Event(EventLine),
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Reply {
    Hello {
        protocol: u32,
        macos_major: u32,
        host_bundle_id: Option<String>,
        permissions: Permissions,
    },
    Apps {
        apps: Vec<AppInfo>,
    },
    Resolved {
        app: AppInfo,
    },
    Launched {
        app: AppInfo,
    },
    Observed {
        observation: Observation,
    },
    Acted {
        outcome: ActOutcome,
    },
    Done,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Permissions {
    pub accessibility: bool,
    pub screen_recording: bool,
    pub event_tap: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct AppInfo {
    pub bundle_id: String,
    pub name: String,
    pub pid: Option<i32>,
    pub category: Option<String>, // LSApplicationCategoryType
    pub windows: Vec<WindowInfo>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct WindowInfo {
    pub id: u32,
    pub title: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Observation {
    pub bundle_id: String,
    pub app_name: String,
    pub window: WindowInfo,
    pub elements: Vec<Element>,
    pub omitted: u32,
    pub no_element_tree: bool,
    pub focused_secure: bool,
    pub screenshot: Option<Screenshot>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Element {
    pub id: u32,
    pub depth: u16,
    pub role: String, // "button", "text field", …
    pub label: Option<String>,
    pub value: Option<String>, // Never set for secure fields
    pub secure: bool,
    pub frame: [i32; 4], // x, y, w, h in screenshot pixels
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Screenshot {
    pub png_base64: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ActOutcome {
    pub foreground_used: bool,
    pub note: Option<String>,
    pub scroll_position: Option<f64>, // 0.0–1.0 when known
    pub observation: Option<Observation>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct DriverError {
    pub kind: ErrorKind,
    pub message: String,
    pub candidates: Vec<AppInfo>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    PermissionMissing,
    VersionMismatch,
    LockHeld,
    AppNotFound,
    AppAmbiguous,
    WindowNotFound,
    Stale,
    NeedsForeground,
    UserActive,
    NeedsSecureApproval,
    ScreenLocked,
    Failed,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serialize;

    fn round_trip<T>(value: T)
    where
        T: Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        let text = serde_json::to_string(&value).unwrap();
        assert_eq!(serde_json::from_str::<T>(&text).unwrap(), value);
    }

    #[test]
    fn a_request_line_round_trips() {
        let line = RequestLine {
            id: 7,
            request: Request::Observe(ObserveParams {
                app: "com.apple.TextEdit".into(),
                window: None,
                query: None,
                max_elements: 300,
                include_screenshot: true,
            }),
        };
        let text = serde_json::to_string(&line).unwrap();
        assert!(text.contains("\"method\":\"observe\""));
        assert_eq!(serde_json::from_str::<RequestLine>(&text).unwrap(), line);
    }

    #[test]
    fn a_driver_line_is_a_response_or_an_event() {
        let event: DriverLine = serde_json::from_str(r#"{"event":"esc"}"#).unwrap();
        assert_eq!(
            event,
            DriverLine::Event(EventLine {
                event: DriverEvent::Esc
            })
        );
        let response: DriverLine =
            serde_json::from_str(r#"{"id":3,"result":{"Ok":{"kind":"done"}}}"#).unwrap();
        assert_eq!(
            response,
            DriverLine::Response(ResponseLine {
                id: 3,
                result: Ok(Reply::Done)
            })
        );
    }

    #[test]
    fn every_request_and_action_round_trips() {
        assert_eq!(PROTOCOL_VERSION, 1);
        let actions = [
            Action::Click {
                target: Target::Element { element: 12 },
                button: Button::Right,
                count: 2,
                modifiers: vec!["cmd".into()],
            },
            Action::Type {
                text: "hello".into(),
                target: None,
            },
            Action::Key {
                keys: vec!["cmd".into(), "s".into()],
                target: Some(Target::Point { point: [1.5, 2.0] }),
            },
            Action::Scroll {
                target: Target::Point { point: [4.0, 5.0] },
                direction: Direction::Down,
                amount: 3,
            },
        ];
        for action in actions {
            round_trip(RequestLine {
                id: 1,
                request: Request::Act(ActParams {
                    app: "x".into(),
                    window: Some(3),
                    action,
                    foreground_fallback: true,
                    observe_after: false,
                    include_screenshot: true,
                    secure_approved: false,
                }),
            });
        }
        for request in [
            Request::Hello { protocol: 1 },
            Request::ListApps,
            Request::ResolveApp {
                app: "x".into(),
                allow_not_running: true,
            },
            Request::LaunchApp {
                app: "x".into(),
                open: vec!["file.rtf".into()],
            },
            Request::Turn { active: true },
            Request::Shutdown,
        ] {
            round_trip(RequestLine { id: 2, request });
        }
    }

    #[test]
    fn every_reply_and_error_kind_round_trips() {
        let app = AppInfo {
            bundle_id: "x".into(),
            name: "Example".into(),
            pid: Some(42),
            category: Some("category".into()),
            windows: vec![WindowInfo {
                id: 1,
                title: "Document".into(),
            }],
        };
        let observation = Observation {
            bundle_id: "x".into(),
            app_name: "Example".into(),
            window: app.windows[0].clone(),
            elements: vec![Element {
                id: 1,
                depth: 2,
                role: "button".into(),
                label: Some("Save".into()),
                value: None,
                secure: false,
                frame: [1, 2, 3, 4],
            }],
            omitted: 3,
            no_element_tree: false,
            focused_secure: true,
            screenshot: Some(Screenshot {
                png_base64: "abc".into(),
                width: 1280,
                height: 902,
            }),
        };
        assert_eq!(
            Permissions::default(),
            Permissions {
                accessibility: false,
                screen_recording: false,
                event_tap: false
            }
        );
        for reply in [
            Reply::Hello {
                protocol: 1,
                macos_major: 26,
                host_bundle_id: Some("host".into()),
                permissions: Permissions::default(),
            },
            Reply::Apps {
                apps: vec![app.clone()],
            },
            Reply::Resolved { app: app.clone() },
            Reply::Launched { app: app.clone() },
            Reply::Observed {
                observation: observation.clone(),
            },
            Reply::Acted {
                outcome: ActOutcome {
                    foreground_used: true,
                    note: Some("note".into()),
                    scroll_position: Some(0.5),
                    observation: Some(observation),
                },
            },
            Reply::Done,
        ] {
            round_trip(DriverLine::Response(ResponseLine {
                id: 4,
                result: Ok(reply),
            }));
        }
        for kind in [
            ErrorKind::PermissionMissing,
            ErrorKind::VersionMismatch,
            ErrorKind::LockHeld,
            ErrorKind::AppNotFound,
            ErrorKind::AppAmbiguous,
            ErrorKind::WindowNotFound,
            ErrorKind::Stale,
            ErrorKind::NeedsForeground,
            ErrorKind::UserActive,
            ErrorKind::NeedsSecureApproval,
            ErrorKind::ScreenLocked,
            ErrorKind::Failed,
        ] {
            round_trip(DriverLine::Response(ResponseLine {
                id: 5,
                result: Err(DriverError {
                    kind,
                    message: "failure".into(),
                    candidates: vec![app.clone()],
                }),
            }));
        }
    }

    #[test]
    fn unknown_wire_variants_are_rejected() {
        for text in [
            r#"{"event":"unknown"}"#,
            r#"{"id":1,"result":{"Ok":{"kind":"unknown"}}}"#,
            r#"{}"#,
        ] {
            assert!(serde_json::from_str::<DriverLine>(text).is_err());
        }
    }
}
