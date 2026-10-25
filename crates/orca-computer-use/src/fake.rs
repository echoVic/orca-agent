//! In-process driver fixtures that use the same pipes and line protocol as a real driver.

use std::io::{self, BufReader};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::client::{DriverLauncher, LaunchedDriver};
use crate::geometry::encode_png;
use crate::observation::filter_elements;
use crate::protocol::*;
use crate::serve::{DriverHandler, LineWriter, serve};

#[derive(Clone)]
struct FakeApp {
    info: AppInfo,
    elements: Vec<Element>,
    focused: u32,
    actions: usize,
    scroll: f64,
}

#[derive(Clone)]
pub struct FakeDesktop {
    pub protocol: u32,
    pub answer_after: Option<Duration>,
    /// Signals request receipt before an artificial reply delay.
    pub request_started: Option<std::sync::mpsc::Sender<Request>>,
    pub background_fails: bool,
    pub user_active: bool,
    pub lock_held: bool,
    pub permissions: Permissions,
    pub turn_active: bool,
    pub last_observe: Option<ObserveParams>,
    pub launch_reply_override: Option<AppInfo>,
    /// The observation after a delivered action finds the window unusable.
    pub post_act_observe_fails: bool,
    /// Window snapshots list the fixture's windows, so windows open before a
    /// turn are the user's. Off by default: snapshots list none and every
    /// window counts as one Orca opened, as tests from before the window
    /// boundary assume.
    pub snapshots_see_windows: bool,
    /// Window snapshots fail, as when ScreenCaptureKit times out.
    pub snapshot_fails: bool,
    apps: Vec<FakeApp>,
}

fn error(kind: ErrorKind, message: impl Into<String>) -> DriverError {
    DriverError {
        kind,
        message: message.into(),
        candidates: vec![],
    }
}

impl FakeDesktop {
    pub fn with_textedit() -> Self {
        Self::with_app("com.apple.TextEdit", "TextEdit")
    }

    pub fn with_textedit_and_calculator() -> Self {
        let mut desktop = Self::with_textedit();
        desktop
            .apps
            .extend(Self::with_app("com.apple.calculator", "Calculator").apps);
        desktop
    }

    pub fn with_two_apps_named(name: &str) -> Self {
        let mut desktop = Self::with_app("com.microsoft.VSCode", name);
        desktop
            .apps
            .extend(Self::with_app("com.todesktop.230313mzl4w4u92", name).apps);
        desktop
    }

    pub fn with_app(bundle_id: &str, name: &str) -> Self {
        let elements = vec![
            Element {
                id: 1,
                depth: 0,
                role: "button".into(),
                label: Some("Bold".into()),
                value: None,
                secure: false,
                frame: [10, 10, 60, 20],
            },
            Element {
                id: 2,
                depth: 0,
                role: "text area".into(),
                label: None,
                value: Some(String::new()),
                secure: false,
                frame: [10, 40, 300, 100],
            },
            Element {
                id: 14,
                depth: 0,
                role: "text field".into(),
                label: Some("Password".into()),
                value: None,
                secure: true,
                frame: [10, 150, 200, 20],
            },
        ];
        Self {
            protocol: PROTOCOL_VERSION,
            answer_after: None,
            request_started: None,
            background_fails: false,
            user_active: false,
            lock_held: false,
            permissions: Permissions {
                accessibility: true,
                screen_recording: true,
                event_tap: true,
            },
            turn_active: false,
            last_observe: None,
            launch_reply_override: None,
            post_act_observe_fails: false,
            snapshots_see_windows: false,
            snapshot_fails: false,
            apps: vec![FakeApp {
                info: AppInfo {
                    bundle_id: bundle_id.into(),
                    name: name.into(),
                    pid: Some(42),
                    category: None,
                    windows: vec![WindowInfo {
                        id: 1,
                        title: if bundle_id == "com.apple.TextEdit" {
                            "probe.rtf".into()
                        } else {
                            name.into()
                        },
                        minimized: false,
                    }],
                },
                elements,
                focused: 2,
                actions: 0,
                scroll: 0.0,
            }],
        }
    }

    /// Adds another window to the fixture's app for window-binding tests.
    /// A running app with no window yet, like TextEdit showing only its Open panel.
    pub fn with_windowless_app(bundle_id: &str, name: &str) -> Self {
        let mut desktop = Self::with_app(bundle_id, name);
        for app in &mut desktop.apps {
            app.info.windows.clear();
        }
        desktop
    }

    pub fn add_window(&mut self, bundle_id: &str, window: WindowInfo) {
        self.apps
            .iter_mut()
            .find(|app| app.info.bundle_id == bundle_id)
            .unwrap()
            .info
            .windows
            .push(window);
    }

    /// The user closed every window; the app keeps running.
    pub fn close_windows(&mut self, bundle_id: &str) {
        for app in self
            .apps
            .iter_mut()
            .filter(|app| app.info.bundle_id == bundle_id)
        {
            app.info.windows.clear();
        }
    }

    pub fn quit_app(&mut self, bundle_id: &str) {
        if let Some(app) = self
            .apps
            .iter_mut()
            .find(|app| app.info.bundle_id == bundle_id)
        {
            app.info.pid = None;
        }
    }

    pub fn actions_on(&self, bundle_id: &str) -> usize {
        self.apps
            .iter()
            .find(|app| app.info.bundle_id == bundle_id)
            .map_or(0, |app| app.actions)
    }

    fn resolve(&self, name: &str, allow_not_running: bool) -> Result<usize, DriverError> {
        let candidates: Vec<_> = self
            .apps
            .iter()
            .enumerate()
            .filter(|(_, app)| allow_not_running || app.info.pid.is_some())
            .filter(|(_, app)| {
                app.info.bundle_id == name || app.info.name.eq_ignore_ascii_case(name)
            })
            .collect();
        match candidates.as_slice() {
            [(index, _)] => Ok(*index),
            [] => Err(error(
                ErrorKind::AppNotFound,
                format!("App not found: {name}"),
            )),
            _ => Err(DriverError {
                kind: ErrorKind::AppAmbiguous,
                message: format!("More than one app is named {name}; use a bundle ID"),
                candidates: candidates.iter().map(|(_, app)| app.info.clone()).collect(),
            }),
        }
    }

    fn running_app(&self, bundle_id: &str, window: Option<u32>) -> Result<usize, DriverError> {
        let index = self
            .apps
            .iter()
            .position(|app| app.info.bundle_id == bundle_id && app.info.pid.is_some())
            .ok_or_else(|| {
                error(
                    ErrorKind::AppNotFound,
                    format!("App is not running: {bundle_id}"),
                )
            })?;
        if let Some(window) = window
            && !self.apps[index]
                .info
                .windows
                .iter()
                .any(|info| info.id == window)
        {
            return Err(error(
                ErrorKind::WindowNotFound,
                crate::messages::not_usable_window(window, &self.apps[index].info.name),
            ));
        }
        Ok(index)
    }

    /// The window observe and act use: the named one unless it is the user's,
    /// else the first that is not the user's.
    fn own_window<'a>(
        app: &'a FakeApp,
        window: Option<u32>,
        user_windows: &[u32],
    ) -> Result<&'a WindowInfo, DriverError> {
        let windows = &app.info.windows;
        let theirs = || {
            windows
                .iter()
                .map(|info| (info.id, info.title.clone()))
                .collect::<Vec<_>>()
        };
        match window {
            Some(id) => {
                let info = windows.iter().find(|info| info.id == id).ok_or_else(|| {
                    error(
                        ErrorKind::WindowNotFound,
                        crate::messages::not_usable_window(id, &app.info.name),
                    )
                })?;
                if user_windows.contains(&id) {
                    return Err(error(
                        ErrorKind::UserWindow,
                        crate::messages::user_window(id, &info.title),
                    ));
                }
                Ok(info)
            }
            None => windows
                .iter()
                .find(|info| !user_windows.contains(&info.id))
                .ok_or_else(|| {
                    error(
                        ErrorKind::UserWindow,
                        crate::messages::user_window_only(&app.info.name, &theirs()),
                    )
                }),
        }
    }

    fn observe(&self, params: &ObserveParams) -> Result<Observation, DriverError> {
        let app = &self.apps[self.running_app(&params.app, params.window)?];
        if params.window.is_none() && app.info.windows.is_empty() {
            return Err(error(
                ErrorKind::WindowNotFound,
                crate::messages::no_window(&app.info.name),
            ));
        }
        let window = Self::own_window(app, params.window, &params.user_windows)?.clone();
        let mut elements = params.query.as_ref().map_or_else(
            || app.elements.clone(),
            |query| filter_elements(&app.elements, query),
        );
        let count = elements.len();
        elements.truncate(params.max_elements.min(1000) as usize);
        let omitted = (count - elements.len()) as u32;
        let screenshot = if params.include_screenshot {
            Some(Screenshot {
                png_base64: encode_png(&image::RgbaImage::from_pixel(
                    320,
                    200,
                    image::Rgba([240, 240, 240, 255]),
                ))
                .map_err(|message| error(ErrorKind::Failed, message))?,
                width: 320,
                height: 200,
            })
        } else {
            None
        };
        Ok(Observation {
            bundle_id: app.info.bundle_id.clone(),
            app_name: app.info.name.clone(),
            window,
            elements,
            omitted,
            no_element_tree: false,
            truncated: false,
            focused_secure: app
                .elements
                .iter()
                .any(|element| element.id == app.focused && element.secure),
            screenshot,
        })
    }

    fn act(&mut self, params: ActParams) -> Result<Reply, DriverError> {
        if !params.user_windows.is_empty()
            && let Action::Key { keys, .. } = &params.action
            && crate::keys::parse_combo(keys)
                .map_err(|message| error(ErrorKind::Failed, message))?
                .acts_on_every_window()
        {
            let index = self.running_app(&params.app, params.window)?;
            return Err(error(
                ErrorKind::UserWindow,
                crate::messages::app_wide_key(&self.apps[index].info.name),
            ));
        }
        // No window of the app's own: none at all, or only the user's. Any
        // app-level shortcut reaches the first; only Cmd+N, which opens a new
        // window, reaches the second.
        if params.window.is_none()
            && let Some(app) = self.apps.iter_mut().find(|app| {
                app.info.bundle_id == params.app
                    && app.info.pid.is_some()
                    && app
                        .info
                        .windows
                        .iter()
                        .all(|window| params.user_windows.contains(&window.id))
            })
        {
            let windowless = app.info.windows.is_empty();
            let stroke = match &params.action {
                Action::Key { keys, target: None } => Some(
                    crate::keys::parse_combo(keys)
                        .map_err(|message| error(ErrorKind::Failed, message))?,
                ),
                _ => None,
            };
            let reaches = stroke.is_some_and(|stroke| {
                if windowless {
                    stroke.is_app_shortcut()
                } else {
                    stroke.opens_new_window()
                }
            });
            if !reaches {
                return Err(if windowless {
                    error(
                        ErrorKind::WindowNotFound,
                        crate::messages::no_window(&app.info.name),
                    )
                } else {
                    let theirs: Vec<_> = app
                        .info
                        .windows
                        .iter()
                        .map(|window| (window.id, window.title.clone()))
                        .collect();
                    error(
                        ErrorKind::UserWindow,
                        crate::messages::user_window_only(&app.info.name, &theirs),
                    )
                });
            }
            if !params.foreground_fallback {
                return Err(error(
                    ErrorKind::NeedsForeground,
                    "A shortcut for an app without a window needs the foreground; the foreground fallback is turned off",
                ));
            }
            app.actions += 1;
            return Ok(Reply::Acted {
                outcome: ActOutcome {
                    foreground_used: true,
                    note: Some(if windowless {
                        "sent to the app, which has no window".into()
                    } else {
                        format!(
                            "sent to the app, whose windows are all the user's; {}",
                            crate::messages::CMD_N_OPENED_NO_WINDOW
                        )
                    }),
                    scroll_position: None,
                    observation: None,
                },
            });
        }
        // Never resolve an action by display name or redirect it to another app.
        let index = self.running_app(&params.app, params.window)?;
        Self::own_window(&self.apps[index], params.window, &params.user_windows)?;
        let target = match &params.action {
            Action::Click { target, .. } | Action::Scroll { target, .. } => Some(*target),
            Action::Type { target, .. } | Action::Key { target, .. } => *target,
        };
        let app = &self.apps[index];
        let element = match target {
            Some(Target::Element { element }) => Some(
                app.elements
                    .iter()
                    .position(|info| info.id == element)
                    .ok_or_else(|| {
                        error(
                            ErrorKind::Stale,
                            format!("Element is no longer available: {element}"),
                        )
                    })?,
            ),
            Some(Target::Point { point: [x, y] }) => {
                if !x.is_finite()
                    || !y.is_finite()
                    || !(0.0..320.0).contains(&x)
                    || !(0.0..200.0).contains(&y)
                {
                    return Err(error(
                        ErrorKind::Stale,
                        "Point is outside the observed window",
                    ));
                }
                app.elements.iter().position(|element| {
                    let [left, top, width, height] = element.frame;
                    x >= f64::from(left)
                        && y >= f64::from(top)
                        && x < f64::from(left + width)
                        && y < f64::from(top + height)
                })
            }
            None => app
                .elements
                .iter()
                .position(|element| element.id == app.focused),
        };
        let enters_text = match &params.action {
            Action::Type { .. } => true,
            Action::Key { keys, .. } => crate::keys::parse_combo(keys)
                .map_err(|message| error(ErrorKind::Failed, message))?
                .enters_text(),
            _ => false,
        };
        if enters_text
            && element.is_some_and(|index| app.elements[index].secure)
            && !params.secure_approved
        {
            return Err(error(
                ErrorKind::NeedsSecureApproval,
                "Typing into a secure field needs separate approval",
            ));
        }
        if self.background_fails {
            if !params.foreground_fallback {
                return Err(error(
                    ErrorKind::NeedsForeground,
                    "Background delivery failed; foreground fallback is disabled",
                ));
            }
            if self.user_active {
                return Err(error(
                    ErrorKind::UserActive,
                    "The user is active; foreground fallback was abandoned",
                ));
            }
        }
        let app = &mut self.apps[index];
        match &params.action {
            Action::Click { .. } => {
                if let Some(index) = element {
                    app.focused = app.elements[index].id;
                }
            }
            Action::Type { text, .. } => {
                if let Some(index) = element
                    && !app.elements[index].secure
                {
                    app.elements[index]
                        .value
                        .get_or_insert_default()
                        .push_str(text);
                }
            }
            Action::Scroll {
                direction, amount, ..
            } => {
                let sign = if matches!(direction, Direction::Up | Direction::Left) {
                    -1.0
                } else {
                    1.0
                };
                app.scroll = (app.scroll + sign * f64::from(*amount) / 10.0).clamp(0.0, 1.0);
            }
            Action::Key { .. } => {}
        }
        app.actions += 1;
        let scroll_position = matches!(params.action, Action::Scroll { .. }).then_some(app.scroll);
        let mut note = None;
        let observation = if params.observe_after {
            let observed = if self.post_act_observe_fails {
                Err(error(
                    ErrorKind::WindowNotFound,
                    crate::messages::not_usable_window(params.window.unwrap_or(0), &params.app),
                ))
            } else {
                self.observe(&ObserveParams {
                    app: params.app,
                    window: params.window,
                    query: None,
                    max_elements: 300,
                    include_screenshot: params.include_screenshot,
                    user_windows: params.user_windows.clone(),
                })
            };
            match observed {
                Ok(observation) => Some(observation),
                // As in the driver: the action happened, so report it, not a failure.
                Err(failure) if failure.kind == ErrorKind::WindowNotFound => {
                    note = Some(crate::messages::WINDOW_UNUSABLE_AFTER_ACTION.to_string());
                    None
                }
                Err(failure) => return Err(failure),
            }
        } else {
            None
        };
        Ok(Reply::Acted {
            outcome: ActOutcome {
                foreground_used: self.background_fails,
                note,
                scroll_position,
                observation,
            },
        })
    }
}

impl DriverHandler for FakeDesktop {
    fn handle(&mut self, request: Request, _events: &LineWriter) -> Result<Reply, DriverError> {
        if let Some(sender) = &self.request_started {
            let _ = sender.send(request.clone());
        }
        if let Some(delay) = self.answer_after {
            thread::sleep(delay);
        }
        match request {
            Request::Hello { .. } => {
                if self.lock_held {
                    return Err(error(
                        ErrorKind::LockHeld,
                        "Another session holds the computer-use lock",
                    ));
                }
                Ok(Reply::Hello {
                    protocol: self.protocol,
                    macos_major: 26,
                    host_bundle_id: Some("com.cmuxterm.app".into()),
                    permissions: self.permissions,
                })
            }
            Request::ListApps => Ok(Reply::Apps {
                apps: self
                    .apps
                    .iter()
                    .filter(|app| app.info.pid.is_some())
                    .map(|app| app.info.clone())
                    .collect(),
            }),
            Request::ResolveApp {
                app,
                allow_not_running,
            } => {
                let index = self.resolve(&app, allow_not_running)?;
                Ok(Reply::Resolved {
                    app: self.apps[index].info.clone(),
                })
            }
            Request::LaunchApp { app, .. } => {
                let index = self.resolve(&app, true)?;
                self.apps[index].info.pid = Some(42 + index as i32);
                Ok(Reply::Launched {
                    app: self
                        .launch_reply_override
                        .clone()
                        .unwrap_or_else(|| self.apps[index].info.clone()),
                })
            }
            Request::Observe(params) => {
                self.last_observe = Some(params.clone());
                Ok(Reply::Observed {
                    observation: self.observe(&params)?,
                })
            }
            Request::Act(params) => self.act(params),
            Request::WindowSnapshot => {
                if self.snapshot_fails {
                    return Err(error(ErrorKind::Failed, "Window listing timed out"));
                }
                Ok(Reply::Apps {
                    apps: self
                        .apps
                        .iter()
                        .filter(|app| app.info.pid.is_some())
                        .map(|app| AppInfo {
                            windows: if self.snapshots_see_windows {
                                app.info.windows.clone()
                            } else {
                                vec![]
                            },
                            ..app.info.clone()
                        })
                        .collect(),
                })
            }
            Request::Turn { active } => {
                self.turn_active = active;
                Ok(Reply::Done)
            }
            Request::Shutdown => Ok(Reply::Done),
        }
    }

    fn shutdown(&mut self) {
        self.turn_active = false;
    }
}

#[derive(Clone)]
pub struct FakeLauncher {
    desktop: Arc<Mutex<FakeDesktop>>,
    launches: Arc<AtomicUsize>,
    shutdowns: Arc<AtomicUsize>,
    events: Arc<Mutex<Option<LineWriter>>>,
}

impl FakeLauncher {
    pub fn new(desktop: FakeDesktop) -> Self {
        Self {
            desktop: Arc::new(Mutex::new(desktop)),
            launches: Arc::new(AtomicUsize::new(0)),
            shutdowns: Arc::new(AtomicUsize::new(0)),
            events: Arc::new(Mutex::new(None)),
        }
    }

    pub fn launches(&self) -> usize {
        self.launches.load(Ordering::SeqCst)
    }
    pub fn shutdowns(&self) -> usize {
        self.shutdowns.load(Ordering::SeqCst)
    }
    pub fn desktop<R>(&self, f: impl FnOnce(&mut FakeDesktop) -> R) -> R {
        f(&mut self.desktop.lock().expect("fake desktop lock poisoned"))
    }
    pub fn emit_esc(&self) {
        if let Some(writer) = self
            .events
            .lock()
            .expect("fake events lock poisoned")
            .as_ref()
        {
            let _ = writer.send(&DriverLine::Event(EventLine {
                event: DriverEvent::Esc,
            }));
        }
    }
}

struct SharedHandler(FakeLauncher);
impl DriverHandler for SharedHandler {
    fn handle(&mut self, request: Request, events: &LineWriter) -> Result<Reply, DriverError> {
        self.0.desktop(|desktop| desktop.handle(request, events))
    }
    fn shutdown(&mut self) {
        self.0.desktop(|desktop| desktop.shutdown());
        self.0.shutdowns.fetch_add(1, Ordering::SeqCst);
    }
}

impl DriverLauncher for FakeLauncher {
    fn launch(&self) -> io::Result<LaunchedDriver> {
        let (input, stdin) = io::pipe()?;
        let (stdout, output) = io::pipe()?;
        let writer = LineWriter::new(output);
        let launcher = self.clone();
        // A launch may follow EOF before the preceding serve thread has finished.
        // Keep cleanup local to that launch so it cannot close a newer event writer.
        let generation = self.launches.fetch_add(1, Ordering::SeqCst) + 1;
        *self.events.lock().expect("fake events lock poisoned") = Some(writer.clone());
        thread::Builder::new()
            .name("fake-computer-use-driver".into())
            .spawn(move || {
                let mut handler = SharedHandler(launcher.clone());
                let _ = serve(BufReader::new(input), writer, &mut handler);
                let mut events = launcher.events.lock().expect("fake events lock poisoned");
                if launcher.launches() == generation {
                    events.take();
                }
            })
            .inspect_err(|_| {
                let mut events = self.events.lock().expect("fake events lock poisoned");
                if self.launches() == generation {
                    events.take();
                }
            })?;
        Ok(LaunchedDriver {
            stdin: Box::new(stdin),
            stdout: Box::new(stdout),
            child: None,
        })
    }
}

#[cfg(test)]
mod regression_tests {
    use super::*;

    fn observe_params(window: Option<u32>, user_windows: Vec<u32>) -> ObserveParams {
        ObserveParams {
            app: "com.apple.TextEdit".into(),
            window,
            query: None,
            max_elements: 10,
            include_screenshot: false,
            user_windows,
        }
    }

    #[test]
    fn the_fake_keeps_off_the_users_windows() {
        let mut desktop = FakeDesktop::with_textedit();
        desktop.add_window(
            "com.apple.TextEdit",
            WindowInfo {
                id: 2,
                title: "Untitled".into(),
                minimized: false,
            },
        );
        // Without a window, the first window that is not the user's.
        assert_eq!(
            desktop
                .observe(&observe_params(None, vec![1]))
                .unwrap()
                .window
                .id,
            2
        );
        let named = desktop
            .observe(&observe_params(Some(1), vec![1]))
            .unwrap_err();
        assert_eq!(
            (named.kind, named.message),
            (
                ErrorKind::UserWindow,
                crate::messages::user_window(1, "probe.rtf")
            )
        );
        let only = desktop
            .observe(&observe_params(None, vec![1, 2]))
            .unwrap_err();
        assert_eq!(
            only.message,
            crate::messages::user_window_only(
                "TextEdit",
                &[(1, "probe.rtf".into()), (2, "Untitled".into())]
            )
        );
    }

    #[test]
    fn app_wide_shortcuts_are_refused_only_while_user_windows_remain() {
        for keys in [
            vec!["cmd", "q"],
            vec!["cmd", "option", "w"],
            vec!["cmd", "option", "m"],
        ] {
            let mut desktop = FakeDesktop::with_textedit();
            desktop.observe(&observe_params(Some(1), vec![])).unwrap();
            let params = ActParams {
                app: "com.apple.TextEdit".into(),
                window: Some(1),
                action: Action::Key {
                    keys: keys.into_iter().map(String::from).collect(),
                    target: None,
                },
                foreground_fallback: true,
                observe_after: false,
                include_screenshot: false,
                secure_approved: false,
                user_windows: vec![2],
            };
            let failure = desktop.act(params.clone()).unwrap_err();
            assert_eq!(failure.kind, ErrorKind::UserWindow);
            assert_eq!(failure.message, crate::messages::app_wide_key("TextEdit"));
            assert_eq!(desktop.actions_on("com.apple.TextEdit"), 0);
            desktop
                .act(ActParams {
                    user_windows: vec![],
                    ..params
                })
                .unwrap();
            assert_eq!(desktop.actions_on("com.apple.TextEdit"), 1);
        }
    }

    #[test]
    fn only_cmd_n_reaches_an_app_whose_windows_are_all_the_users() {
        let mut desktop = FakeDesktop::with_textedit();
        let key = |keys: &[&str]| ActParams {
            app: "com.apple.TextEdit".into(),
            window: None,
            action: Action::Key {
                keys: keys.iter().map(|k| k.to_string()).collect(),
                target: None,
            },
            foreground_fallback: true,
            observe_after: false,
            include_screenshot: false,
            secure_approved: false,
            user_windows: vec![1],
        };
        let Ok(Reply::Acted { outcome }) = desktop.act(key(&["cmd", "n"])) else {
            panic!("cmd+n should reach the app")
        };
        assert!(outcome.foreground_used);
        assert!(outcome.observation.is_none());
        assert_eq!(
            outcome.note.as_deref(),
            Some(
                "sent to the app, whose windows are all the user's; Cmd+N opened no new window; the app may have acted in the user's window instead. Observe again before going on."
            )
        );
        assert_eq!(desktop.actions_on("com.apple.TextEdit"), 1);
        let refused = desktop.act(key(&["cmd", "w"])).unwrap_err();
        assert_eq!(refused.kind, ErrorKind::UserWindow);
        assert_eq!(desktop.actions_on("com.apple.TextEdit"), 1);
    }

    #[test]
    fn window_snapshots_list_windows_only_when_the_fixture_says_they_predate_the_turn() {
        let mut desktop = FakeDesktop::with_textedit();
        let output = LineWriter::new(std::io::sink());
        let ids = |desktop: &mut FakeDesktop| match desktop.handle(Request::WindowSnapshot, &output)
        {
            Ok(Reply::Apps { apps }) => apps
                .into_iter()
                .flat_map(|app| app.windows)
                .map(|window| window.id)
                .collect::<Vec<_>>(),
            other => panic!("expected a snapshot: {other:?}"),
        };
        assert!(ids(&mut desktop).is_empty());
        desktop.snapshots_see_windows = true;
        assert_eq!(ids(&mut desktop), vec![1]);
        desktop.snapshot_fails = true;
        assert!(desktop.handle(Request::WindowSnapshot, &output).is_err());
    }

    #[test]
    fn a_windowless_app_accepts_only_an_app_shortcut_without_a_window() {
        let mut desktop = FakeDesktop::with_windowless_app("com.apple.TextEdit", "TextEdit");
        let key = |keys: &[&str], window| ActParams {
            app: "com.apple.TextEdit".into(),
            window,
            action: Action::Key {
                keys: keys.iter().map(|k| k.to_string()).collect(),
                target: None,
            },
            foreground_fallback: true,
            observe_after: false,
            include_screenshot: false,
            secure_approved: false,
            user_windows: vec![],
        };
        let Ok(Reply::Acted { outcome }) = desktop.act(key(&["cmd", "n"], None)) else {
            panic!("cmd+n should reach a windowless app")
        };
        assert!(outcome.foreground_used);
        let error = desktop.act(key(&["a"], None)).unwrap_err();
        assert_eq!(error.message, crate::messages::no_window("TextEdit"));
        let mut off = key(&["cmd", "n"], None);
        off.foreground_fallback = false;
        assert_eq!(
            desktop.act(off).unwrap_err().kind,
            ErrorKind::NeedsForeground
        );
    }

    #[test]
    fn fake_window_errors_use_the_shared_messages() {
        let mut desktop = FakeDesktop::with_textedit();
        let observe = |window| ObserveParams {
            app: "com.apple.TextEdit".into(),
            window,
            query: None,
            max_elements: 10,
            include_screenshot: false,
            user_windows: vec![],
        };
        let error = desktop.observe(&observe(Some(999))).unwrap_err();
        assert_eq!(error.kind, ErrorKind::WindowNotFound);
        assert_eq!(
            error.message,
            crate::messages::not_usable_window(999, "TextEdit")
        );
        desktop.apps[0].info.windows.clear();
        let error = desktop.observe(&observe(None)).unwrap_err();
        assert_eq!(error.message, crate::messages::no_window("TextEdit"));
    }

    #[test]
    fn secure_fields_gate_text_keys_but_not_tab() {
        for (keys, approval_needed) in [
            (vec!["p"], true),
            (vec!["cmd", "v"], true),
            (vec!["tab"], false),
        ] {
            let mut desktop = FakeDesktop::with_textedit();
            let result = desktop.act(ActParams {
                app: "com.apple.TextEdit".into(),
                window: None,
                action: Action::Key {
                    keys: keys.iter().map(|key| (*key).into()).collect(),
                    target: Some(Target::Element { element: 14 }),
                },
                foreground_fallback: true,
                observe_after: false,
                include_screenshot: false,
                secure_approved: false,
                user_windows: vec![],
            });
            if approval_needed {
                assert_eq!(result.unwrap_err().kind, ErrorKind::NeedsSecureApproval);
            } else {
                assert!(result.is_ok(), "{result:?}");
            }
        }
    }

    #[test]
    fn observing_window_b_returns_window_b() {
        let mut desktop = FakeDesktop::with_textedit();
        desktop.add_window(
            "com.apple.TextEdit",
            WindowInfo {
                id: 2,
                title: "B".into(),
                minimized: false,
            },
        );
        let observation = desktop
            .observe(&ObserveParams {
                app: "com.apple.TextEdit".into(),
                window: Some(2),
                query: None,
                max_elements: 300,
                include_screenshot: false,
                user_windows: vec![],
            })
            .unwrap();
        assert_eq!(observation.window.id, 2);
    }
}
