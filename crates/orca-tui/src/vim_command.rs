use ratatui_textarea::{Input, Key};

pub(crate) const MAX_VIM_COUNT: usize = 9_999;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VimRegisterSelector {
    Unnamed,
    Named(u8),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VimMotion {
    Back,
    Down,
    Up,
    Forward,
    WordForward,
    WordEnd,
    WordBack,
    LineHead,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VimCommand {
    Motion {
        motion: VimMotion,
        count: usize,
    },
    DeleteChars {
        count: usize,
        register: VimRegisterSelector,
    },
    DeleteToEnd {
        register: VimRegisterSelector,
    },
    ChangeToEnd {
        register: VimRegisterSelector,
    },
    DeleteLines {
        count: usize,
        register: VimRegisterSelector,
    },
    YankLines {
        count: usize,
        register: VimRegisterSelector,
    },
    Paste {
        count: usize,
        register: VimRegisterSelector,
    },
    GotoLine {
        one_based: Option<usize>,
    },
    Repeat {
        count: usize,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VimCommandResolution {
    Pending,
    Execute(VimCommand),
    Consumed,
    Unhandled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VimVisualCommand {
    Yank(VimRegisterSelector),
    Delete(VimRegisterSelector),
    Change(VimRegisterSelector),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VimVisualResolution {
    Pending,
    Execute(VimVisualCommand),
    Consumed,
    Unhandled,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum VimPendingPrefix {
    #[default]
    None,
    Register,
    DeleteLine,
    YankLine,
    Goto,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct VimCommandParser {
    count: Option<usize>,
    selected_register: Option<VimRegisterSelector>,
    pending: VimPendingPrefix,
}

impl VimCommandParser {
    pub(crate) fn resolve_normal(&mut self, input: Input) -> VimCommandResolution {
        if input.ctrl || input.alt {
            self.reset();
            return VimCommandResolution::Unhandled;
        }
        let Key::Char(character) = input.key else {
            self.reset();
            return VimCommandResolution::Unhandled;
        };

        match self.pending {
            VimPendingPrefix::Register => {
                let Some(register) = Self::accept_register(character) else {
                    return self.consume();
                };
                self.selected_register = Some(register);
                self.pending = VimPendingPrefix::None;
                return VimCommandResolution::Pending;
            }
            VimPendingPrefix::DeleteLine => {
                if character != 'd' {
                    return self.consume();
                }
                let command = VimCommand::DeleteLines {
                    count: self.take_count(),
                    register: self.take_register(),
                };
                return self.finish(command);
            }
            VimPendingPrefix::YankLine => {
                if character != 'y' {
                    return self.consume();
                }
                let command = VimCommand::YankLines {
                    count: self.take_count(),
                    register: self.take_register(),
                };
                return self.finish(command);
            }
            VimPendingPrefix::Goto => {
                if character != 'g' || self.selected_register.is_some() {
                    return self.consume();
                }
                let one_based = Some(self.take_count());
                return self.finish(VimCommand::GotoLine { one_based });
            }
            VimPendingPrefix::None => {}
        }

        if character.is_ascii_digit() {
            let digit = character.to_digit(10).unwrap_or_default() as usize;
            if digit != 0 || self.count.is_some() {
                self.append_count(digit);
                return VimCommandResolution::Pending;
            }
        }

        match character {
            '"' => {
                self.pending = VimPendingPrefix::Register;
                VimCommandResolution::Pending
            }
            'd' => {
                self.pending = VimPendingPrefix::DeleteLine;
                VimCommandResolution::Pending
            }
            'y' => {
                self.pending = VimPendingPrefix::YankLine;
                VimCommandResolution::Pending
            }
            'g' if self.selected_register.is_none() => {
                self.pending = VimPendingPrefix::Goto;
                VimCommandResolution::Pending
            }
            _ if self.selected_register.is_some()
                && !matches!(character, 'x' | 'D' | 'C' | 'p') =>
            {
                self.consume()
            }
            'h' | 'j' | 'k' | 'l' | 'w' | 'e' | 'b' | '0' => {
                let motion = match character {
                    'h' => VimMotion::Back,
                    'j' => VimMotion::Down,
                    'k' => VimMotion::Up,
                    'l' => VimMotion::Forward,
                    'w' => VimMotion::WordForward,
                    'e' => VimMotion::WordEnd,
                    'b' => VimMotion::WordBack,
                    '0' => VimMotion::LineHead,
                    _ => unreachable!(),
                };
                let count = self.take_count();
                self.finish(VimCommand::Motion { motion, count })
            }
            'x' => {
                let command = VimCommand::DeleteChars {
                    count: self.take_count(),
                    register: self.take_register(),
                };
                self.finish(command)
            }
            'D' => {
                self.count = None;
                let register = self.take_register();
                self.finish(VimCommand::DeleteToEnd { register })
            }
            'C' => {
                self.count = None;
                let register = self.take_register();
                self.finish(VimCommand::ChangeToEnd { register })
            }
            'p' => {
                let command = VimCommand::Paste {
                    count: self.take_count(),
                    register: self.take_register(),
                };
                self.finish(command)
            }
            'G' if self.selected_register.is_none() => {
                let one_based = self.count.take();
                self.finish(VimCommand::GotoLine { one_based })
            }
            '.' if self.selected_register.is_none() => {
                let count = self.take_count();
                self.finish(VimCommand::Repeat { count })
            }
            _ => {
                self.reset();
                VimCommandResolution::Unhandled
            }
        }
    }

    pub(crate) fn resolve_visual(&mut self, input: Input) -> VimVisualResolution {
        if input.ctrl || input.alt {
            self.reset();
            return VimVisualResolution::Unhandled;
        }
        let Key::Char(character) = input.key else {
            self.reset();
            return VimVisualResolution::Unhandled;
        };

        if self.pending == VimPendingPrefix::Register {
            let Some(register) = Self::accept_register(character) else {
                self.reset();
                return VimVisualResolution::Consumed;
            };
            self.selected_register = Some(register);
            self.pending = VimPendingPrefix::None;
            return VimVisualResolution::Pending;
        }

        if self.pending != VimPendingPrefix::None || self.count.is_some() {
            self.reset();
            return VimVisualResolution::Consumed;
        }

        if character == '"' {
            self.reset();
            self.pending = VimPendingPrefix::Register;
            return VimVisualResolution::Pending;
        }

        let command = match character {
            'y' => Some(VimVisualCommand::Yank(self.take_register())),
            'd' => Some(VimVisualCommand::Delete(self.take_register())),
            'c' => Some(VimVisualCommand::Change(self.take_register())),
            _ => None,
        };
        if let Some(command) = command {
            self.reset();
            return VimVisualResolution::Execute(command);
        }
        if self.selected_register.is_some() {
            self.reset();
            VimVisualResolution::Consumed
        } else {
            VimVisualResolution::Unhandled
        }
    }

    pub(crate) fn reset(&mut self) {
        self.count = None;
        self.selected_register = None;
        self.pending = VimPendingPrefix::None;
    }

    fn append_count(&mut self, digit: usize) {
        self.count = Some(
            self.count
                .unwrap_or(0)
                .saturating_mul(10)
                .saturating_add(digit)
                .min(MAX_VIM_COUNT),
        );
    }

    fn take_count(&mut self) -> usize {
        self.count.take().unwrap_or(1)
    }

    fn take_register(&mut self) -> VimRegisterSelector {
        self.selected_register
            .take()
            .unwrap_or(VimRegisterSelector::Unnamed)
    }

    fn finish(&mut self, command: VimCommand) -> VimCommandResolution {
        self.pending = VimPendingPrefix::None;
        self.count = None;
        self.selected_register = None;
        VimCommandResolution::Execute(command)
    }

    fn consume(&mut self) -> VimCommandResolution {
        self.reset();
        VimCommandResolution::Consumed
    }

    fn accept_register(character: char) -> Option<VimRegisterSelector> {
        match character {
            '"' => Some(VimRegisterSelector::Unnamed),
            'a'..='z' => Some(VimRegisterSelector::Named(
                (character as u8).saturating_sub(b'a'),
            )),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn has_pending(&self) -> bool {
        self.count.is_some()
            || self.selected_register.is_some()
            || self.pending != VimPendingPrefix::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(character: char) -> Input {
        Input {
            key: Key::Char(character),
            ctrl: false,
            alt: false,
            shift: character.is_ascii_uppercase(),
        }
    }

    fn modified(character: char) -> Input {
        Input {
            key: Key::Char(character),
            ctrl: true,
            alt: false,
            shift: false,
        }
    }

    fn resolve_sequence(sequence: &str) -> VimCommandResolution {
        let mut parser = VimCommandParser::default();
        let mut resolution = VimCommandResolution::Unhandled;
        for character in sequence.chars() {
            resolution = parser.resolve_normal(plain(character));
        }
        resolution
    }

    #[test]
    fn parses_counts_line_commands_registers_and_repeat() {
        assert_eq!(
            resolve_sequence("3h"),
            VimCommandResolution::Execute(VimCommand::Motion {
                motion: VimMotion::Back,
                count: 3,
            })
        );
        assert_eq!(
            resolve_sequence("2dd"),
            VimCommandResolution::Execute(VimCommand::DeleteLines {
                count: 2,
                register: VimRegisterSelector::Unnamed,
            })
        );
        assert_eq!(
            resolve_sequence("2yy"),
            VimCommandResolution::Execute(VimCommand::YankLines {
                count: 2,
                register: VimRegisterSelector::Unnamed,
            })
        );
        assert_eq!(
            resolve_sequence("4gg"),
            VimCommandResolution::Execute(VimCommand::GotoLine { one_based: Some(4) })
        );
        assert_eq!(
            resolve_sequence("G"),
            VimCommandResolution::Execute(VimCommand::GotoLine { one_based: None })
        );
        assert_eq!(
            resolve_sequence("3G"),
            VimCommandResolution::Execute(VimCommand::GotoLine { one_based: Some(3) })
        );
        assert_eq!(
            resolve_sequence("\"add"),
            VimCommandResolution::Execute(VimCommand::DeleteLines {
                count: 1,
                register: VimRegisterSelector::Named(0),
            })
        );
        assert_eq!(
            resolve_sequence("\"ayy"),
            VimCommandResolution::Execute(VimCommand::YankLines {
                count: 1,
                register: VimRegisterSelector::Named(0),
            })
        );
        assert_eq!(
            resolve_sequence("\"ap"),
            VimCommandResolution::Execute(VimCommand::Paste {
                count: 1,
                register: VimRegisterSelector::Named(0),
            })
        );
        assert_eq!(
            resolve_sequence("\"\"p"),
            VimCommandResolution::Execute(VimCommand::Paste {
                count: 1,
                register: VimRegisterSelector::Unnamed,
            })
        );
        assert_eq!(
            resolve_sequence("3."),
            VimCommandResolution::Execute(VimCommand::Repeat { count: 3 })
        );
    }

    #[test]
    fn count_and_register_prefixes_work_in_either_order() {
        for sequence in ["2\"add", "\"a2dd"] {
            assert_eq!(
                resolve_sequence(sequence),
                VimCommandResolution::Execute(VimCommand::DeleteLines {
                    count: 2,
                    register: VimRegisterSelector::Named(0),
                }),
                "{sequence}"
            );
        }
    }

    #[test]
    fn bare_zero_is_line_head_and_zero_extends_an_existing_count() {
        assert_eq!(
            resolve_sequence("0"),
            VimCommandResolution::Execute(VimCommand::Motion {
                motion: VimMotion::LineHead,
                count: 1,
            })
        );
        assert_eq!(
            resolve_sequence("20x"),
            VimCommandResolution::Execute(VimCommand::DeleteChars {
                count: 20,
                register: VimRegisterSelector::Unnamed,
            })
        );
    }

    #[test]
    fn count_saturates_at_the_hard_limit() {
        assert_eq!(
            resolve_sequence("999999x"),
            VimCommandResolution::Execute(VimCommand::DeleteChars {
                count: MAX_VIM_COUNT,
                register: VimRegisterSelector::Unnamed,
            })
        );
    }

    #[test]
    fn invalid_pending_continuations_are_consumed_and_reset() {
        for sequence in ["dx", "yx", "gx", "\"1", "d2"] {
            let mut parser = VimCommandParser::default();
            let mut resolution = VimCommandResolution::Unhandled;
            for character in sequence.chars() {
                resolution = parser.resolve_normal(plain(character));
            }
            assert_eq!(resolution, VimCommandResolution::Consumed, "{sequence}");
            assert!(!parser.has_pending(), "{sequence}");
        }
    }

    #[test]
    fn explicit_register_rejects_non_register_commands() {
        assert_eq!(resolve_sequence("\"ah"), VimCommandResolution::Consumed);
    }

    #[test]
    fn modified_input_clears_pending_and_remains_unhandled() {
        let mut parser = VimCommandParser::default();
        assert_eq!(
            parser.resolve_normal(plain('2')),
            VimCommandResolution::Pending
        );
        assert_eq!(
            parser.resolve_normal(modified('r')),
            VimCommandResolution::Unhandled
        );
        assert!(!parser.has_pending());
    }

    #[test]
    fn visual_parser_supports_only_register_prefix_and_visual_operations() {
        let mut parser = VimCommandParser::default();
        assert_eq!(
            parser.resolve_visual(plain('"')),
            VimVisualResolution::Pending
        );
        assert_eq!(
            parser.resolve_visual(plain('b')),
            VimVisualResolution::Pending
        );
        assert_eq!(
            parser.resolve_visual(plain('d')),
            VimVisualResolution::Execute(VimVisualCommand::Delete(VimRegisterSelector::Named(1)))
        );
        assert!(!parser.has_pending());
    }
}
