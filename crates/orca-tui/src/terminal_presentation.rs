#![cfg_attr(not(test), allow(dead_code))]

use std::collections::VecDeque;
use std::io::{self, Write};

use qwertty::{Multiplexer, TerminalIdentity, TerminalProgram};

use crate::selection::tmux_passthrough;
use crate::types::AppStatus;

const MAX_PENDING_NOTIFICATIONS: usize = 32;
const MAX_NOTIFICATIONS_PER_WRITE: usize = 8;
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TerminalPresentationProfile {
    pub(crate) osc9_supported: bool,
    pub(crate) tmux_passthrough: bool,
}

impl TerminalPresentationProfile {
    /// The profile of the terminal this process runs in, as its environment
    /// tells it: what the terminal session presents with, and what a forced
    /// exit puts the window title back with (`termination_signals`).
    pub(crate) fn from_env() -> Self {
        Self::from_identity(&qwertty::caps::identity_from_env(
            None,
            qwertty::caps::std_env_source,
        ))
    }

    pub(crate) fn from_identity(identity: &TerminalIdentity) -> Self {
        let osc9_supported = matches!(
            identity.program,
            Some(
                TerminalProgram::Ghostty
                    | TerminalProgram::Iterm2
                    | TerminalProgram::Kitty
                    | TerminalProgram::WezTerm
            )
        );
        let tmux_passthrough = identity
            .mux_stack
            .iter()
            .any(|multiplexer| matches!(multiplexer, Multiplexer::Tmux));
        Self {
            osc9_supported,
            tmux_passthrough,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TerminalNotification {
    message: String,
}

impl TerminalNotification {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

pub(crate) struct TerminalPresentation {
    focused: bool,
    notifications_enabled: bool,
    output_failed: bool,
    profile: TerminalPresentationProfile,
    animation_tick: u64,
    last_title: Option<String>,
    pending_notifications: VecDeque<TerminalNotification>,
}

impl TerminalPresentation {
    pub(crate) fn new(notifications_enabled: bool, profile: TerminalPresentationProfile) -> Self {
        Self {
            focused: true,
            notifications_enabled,
            output_failed: false,
            profile,
            animation_tick: 0,
            last_title: None,
            pending_notifications: VecDeque::new(),
        }
    }

    pub(crate) fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    pub(crate) const fn is_focused(&self) -> bool {
        self.focused
    }

    pub(crate) fn enqueue(&mut self, notification: TerminalNotification) {
        if !self.notifications_enabled || self.focused {
            return;
        }
        if self.pending_notifications.back() == Some(&notification) {
            return;
        }
        if self.pending_notifications.len() == MAX_PENDING_NOTIFICATIONS {
            self.pending_notifications.pop_front();
        }
        self.pending_notifications.push_back(notification);
    }

    pub(crate) fn advance_tick(&mut self) {
        self.animation_tick = self.animation_tick.wrapping_add(1);
    }

    pub(crate) fn animation_active(&self, status: AppStatus) -> bool {
        matches!(
            status,
            AppStatus::Running | AppStatus::Compacting | AppStatus::WaitingApproval
        )
    }

    pub(crate) fn title(&self, status: AppStatus) -> String {
        let spinner = SPINNER_FRAMES[self.animation_tick as usize % SPINNER_FRAMES.len()];
        match status {
            AppStatus::Running => format!("{spinner} Orca"),
            AppStatus::Compacting => format!("{spinner} Orca · compacting"),
            AppStatus::WaitingApproval if (self.animation_tick / 6).is_multiple_of(2) => {
                "[!] Orca".to_string()
            }
            AppStatus::WaitingUserInput => "[?] Orca".to_string(),
            AppStatus::Setup
            | AppStatus::SessionPicker
            | AppStatus::Idle
            | AppStatus::WaitingApproval => "Orca".to_string(),
        }
    }

    pub(crate) fn invalidate_title(&mut self) {
        self.last_title = None;
    }

    pub(crate) fn write_pending<W: Write>(
        &mut self,
        writer: &mut W,
        status: AppStatus,
    ) -> io::Result<usize> {
        if self.output_failed {
            return Ok(0);
        }

        let result = (|| {
            let title = self.title(status);
            let mut writes = 0usize;
            if self.last_title.as_deref() != Some(title.as_str()) {
                writer.write_all(&encode_title(&title, self.profile))?;
                self.last_title = Some(title);
                writes += 1;
            }

            for _ in 0..MAX_NOTIFICATIONS_PER_WRITE {
                let Some(notification) = self.pending_notifications.pop_front() else {
                    break;
                };
                writer.write_all(&encode_notification(&notification.message, self.profile))?;
                writes += 1;
            }
            if writes > 0 {
                writer.flush()?;
            }
            Ok(writes)
        })();
        if result.is_err() {
            self.output_failed = true;
            self.pending_notifications.clear();
        }
        result
    }

    pub(crate) fn write_reset_title<W: Write>(&mut self, writer: &mut W) -> io::Result<usize> {
        if self.output_failed {
            return Ok(0);
        }
        let result = (|| {
            writer.write_all(&encode_title("Orca", self.profile))?;
            writer.flush()?;
            self.last_title = Some("Orca".to_string());
            Ok(1)
        })();
        if result.is_err() {
            self.output_failed = true;
            self.pending_notifications.clear();
        }
        result
    }

    #[cfg(test)]
    fn pending_len(&self) -> usize {
        self.pending_notifications.len()
    }

    #[cfg(test)]
    pub(crate) fn pending_len_for_test(&self) -> usize {
        self.pending_notifications.len()
    }
}

pub(crate) fn encode_notification(message: &str, profile: TerminalPresentationProfile) -> Vec<u8> {
    if !profile.osc9_supported && !profile.tmux_passthrough {
        return vec![0x07];
    }
    let message = qwertty::commands::osc::sanitize_title(message);
    let sequence = format!("\x1b]9;{message}\x1b\\");
    if profile.tmux_passthrough {
        tmux_passthrough(&sequence).into_bytes()
    } else {
        sequence.into_bytes()
    }
}

pub(crate) fn encode_title(title: &str, profile: TerminalPresentationProfile) -> Vec<u8> {
    let mut buffer = qwertty::CommandBuffer::new();
    buffer.command(qwertty::commands::osc::set_icon_and_title(title));
    let sequence = String::from_utf8(buffer.into_bytes())
        .expect("qwertty OSC title commands contain valid UTF-8");
    if profile.tmux_passthrough {
        tmux_passthrough(&sequence).into_bytes()
    } else {
        sequence.into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};

    use qwertty::caps::identity_from_env;

    use super::{
        TerminalNotification, TerminalPresentation, TerminalPresentationProfile,
        encode_notification, encode_title,
    };
    use crate::types::AppStatus;

    fn identity(values: &[(&str, &str)]) -> qwertty::TerminalIdentity {
        identity_from_env(None, |key| {
            values
                .iter()
                .find_map(|(candidate, value)| (*candidate == key).then(|| (*value).to_string()))
        })
    }

    fn direct_profile() -> TerminalPresentationProfile {
        TerminalPresentationProfile {
            osc9_supported: true,
            tmux_passthrough: false,
        }
    }

    fn unknown_profile() -> TerminalPresentationProfile {
        TerminalPresentationProfile {
            osc9_supported: false,
            tmux_passthrough: false,
        }
    }

    fn presentation(profile: TerminalPresentationProfile) -> TerminalPresentation {
        TerminalPresentation::new(true, profile)
    }

    #[test]
    fn terminal_presentation_profile_classifies_known_programs_and_tmux() {
        for environment in [
            [("TERM_PROGRAM", "ghostty")],
            [("TERM_PROGRAM", "iTerm.app")],
            [("TERM", "xterm-kitty")],
            [("TERM_PROGRAM", "WezTerm")],
        ] {
            assert_eq!(
                TerminalPresentationProfile::from_identity(&identity(&environment)),
                direct_profile()
            );
        }

        for environment in [
            [("TERM", "alacritty")],
            [("TERM_PROGRAM", "Apple_Terminal")],
            [("TERM", "mystery")],
        ] {
            assert_eq!(
                TerminalPresentationProfile::from_identity(&identity(&environment)),
                unknown_profile()
            );
        }

        assert_eq!(
            TerminalPresentationProfile::from_identity(&identity(&[("TMUX", "x")])),
            TerminalPresentationProfile {
                osc9_supported: false,
                tmux_passthrough: true,
            }
        );
    }

    #[test]
    fn terminal_presentation_encodes_osc9_bel_osc0_and_tmux() {
        assert_eq!(
            encode_notification("done", direct_profile()),
            b"\x1b]9;done\x1b\\".to_vec()
        );
        assert_eq!(
            encode_notification("done", unknown_profile()),
            b"\x07".to_vec()
        );
        assert_eq!(
            encode_title("Orca", direct_profile()),
            b"\x1b]0;Orca\x1b\\".to_vec()
        );

        let tmux = TerminalPresentationProfile {
            osc9_supported: false,
            tmux_passthrough: true,
        };
        assert_eq!(
            encode_notification("done", tmux),
            b"\x1bPtmux;\x1b\x1b]9;done\x1b\x1b\\\x1b\\".to_vec()
        );
        assert_eq!(
            encode_title("Orca", tmux),
            b"\x1bPtmux;\x1b\x1b]0;Orca\x1b\x1b\\\x1b\\".to_vec()
        );
    }

    #[test]
    fn terminal_presentation_sanitizes_and_bounds_notification_text() {
        let malicious = format!("safe\x1b]0;spoof\x07\u{202e}{}", "x".repeat(400));
        let encoded = String::from_utf8(encode_notification(&malicious, direct_profile())).unwrap();

        assert!(encoded.starts_with("\x1b]9;safe]0;spoof"));
        assert!(encoded.ends_with("\x1b\\"));
        assert!(!encoded.contains('\u{202e}'));
        assert!(!encoded[4..encoded.len() - 2].contains('\x1b'));
        assert!(encoded.chars().count() <= 4 + 240 + 2);
    }

    #[test]
    fn terminal_presentation_title_matrix_and_animation_are_stable() {
        let mut presentation = presentation(direct_profile());

        for status in [AppStatus::Setup, AppStatus::SessionPicker, AppStatus::Idle] {
            assert_eq!(presentation.title(status), "Orca");
        }
        assert_eq!(presentation.title(AppStatus::Running), "⠋ Orca");
        assert_eq!(
            presentation.title(AppStatus::Compacting),
            "⠋ Orca · compacting"
        );
        assert_eq!(presentation.title(AppStatus::WaitingApproval), "[!] Orca");
        assert_eq!(presentation.title(AppStatus::WaitingUserInput), "[?] Orca");

        for _ in 0..6 {
            presentation.advance_tick();
        }
        assert_eq!(presentation.title(AppStatus::WaitingApproval), "Orca");
        assert_eq!(presentation.title(AppStatus::Running), "⠦ Orca");
        assert!(presentation.animation_active(AppStatus::Running));
        assert!(presentation.animation_active(AppStatus::Compacting));
        assert!(presentation.animation_active(AppStatus::WaitingApproval));
        assert!(!presentation.animation_active(AppStatus::Idle));
        assert!(!presentation.animation_active(AppStatus::WaitingUserInput));
    }

    #[test]
    fn terminal_presentation_queue_is_bounded_deduplicated_and_drained_eight_at_a_time() {
        let mut presentation = presentation(direct_profile());
        presentation.set_focused(false);

        presentation.enqueue(TerminalNotification::new("same"));
        presentation.enqueue(TerminalNotification::new("same"));
        assert_eq!(presentation.pending_len(), 1);

        for index in 0..40 {
            presentation.enqueue(TerminalNotification::new(format!("item-{index}")));
        }
        assert_eq!(presentation.pending_len(), 32);

        let mut output = Vec::new();
        let writes = presentation
            .write_pending(&mut output, AppStatus::Idle)
            .unwrap();
        assert_eq!(writes, 9, "one title plus eight notifications");
        assert_eq!(presentation.pending_len(), 24);
        assert_eq!(
            output
                .windows(4)
                .filter(|bytes| *bytes == b"\x1b]9;")
                .count(),
            8
        );
    }

    #[test]
    fn terminal_presentation_suppresses_focused_and_disabled_notifications() {
        let mut presentation = presentation(direct_profile());
        presentation.enqueue(TerminalNotification::new("focused"));
        assert_eq!(presentation.pending_len(), 0);

        presentation.set_focused(false);
        presentation.enqueue(TerminalNotification::new("unfocused"));
        assert_eq!(presentation.pending_len(), 1);

        let mut disabled = TerminalPresentation::new(false, direct_profile());
        disabled.set_focused(false);
        disabled.enqueue(TerminalNotification::new("disabled"));
        assert_eq!(disabled.pending_len(), 0);
    }

    #[test]
    fn terminal_presentation_deduplicates_title_and_invalidation_reemits() {
        let mut presentation = presentation(direct_profile());
        let mut output = Vec::new();

        assert_eq!(
            presentation
                .write_pending(&mut output, AppStatus::Idle)
                .unwrap(),
            1
        );
        assert_eq!(
            presentation
                .write_pending(&mut output, AppStatus::Idle)
                .unwrap(),
            0
        );
        presentation.invalidate_title();
        assert_eq!(
            presentation
                .write_pending(&mut output, AppStatus::Idle)
                .unwrap(),
            1
        );
        assert_eq!(
            output
                .windows(4)
                .filter(|bytes| *bytes == b"\x1b]0;")
                .count(),
            2
        );
    }

    struct FailingWriter {
        writes: usize,
    }

    impl Write for FailingWriter {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            self.writes += 1;
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "writer failed"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn terminal_presentation_writer_returns_first_error_without_retrying() {
        let mut presentation = presentation(direct_profile());
        let mut writer = FailingWriter { writes: 0 };
        let error = presentation
            .write_pending(&mut writer, AppStatus::Running)
            .expect_err("writer should fail");

        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(writer.writes, 1);
    }

    #[test]
    fn terminal_presentation_disables_all_output_after_first_write_error() {
        let mut presentation = presentation(direct_profile());
        presentation.set_focused(false);
        presentation.enqueue(TerminalNotification::new("Approval required"));
        let mut writer = FailingWriter { writes: 0 };

        assert!(
            presentation
                .write_pending(&mut writer, AppStatus::Running)
                .is_err()
        );
        assert_eq!(
            presentation
                .write_pending(&mut writer, AppStatus::Running)
                .unwrap(),
            0
        );
        assert_eq!(presentation.write_reset_title(&mut writer).unwrap(), 0);
        assert_eq!(writer.writes, 1, "failed output must never be retried");
        assert_eq!(
            presentation.pending_len(),
            0,
            "undeliverable notifications are discarded"
        );
    }

    #[test]
    fn terminal_presentation_reset_title_writes_orca_even_after_orca_was_cached() {
        let mut presentation = presentation(direct_profile());
        let mut output = Vec::new();
        presentation
            .write_pending(&mut output, AppStatus::Idle)
            .unwrap();
        let before_reset = output.len();

        assert_eq!(presentation.write_reset_title(&mut output).unwrap(), 1);
        assert_eq!(
            &output[before_reset..],
            b"\x1b]0;Orca\x1b\\",
            "orderly exit always reasserts the safe title"
        );
    }
}
