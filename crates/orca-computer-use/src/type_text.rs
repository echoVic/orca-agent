//! Validation for literal text insertion, shared by the parser and native delivery.
pub(crate) fn validate_controls(text: &str) -> Result<(), String> {
    if let Some(control) = text.chars().find(|c| {
        ((*c as u32) < 32 && !matches!(c, '\t' | '\n' | '\r'))
            || *c == '\u{7f}'
            || ('\u{f700}'..='\u{f8ff}').contains(c)
    }) {
        return Err(format!(
            "type cannot insert control U+{:04X}; use key for keyboard commands",
            control as u32
        ));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub(crate) fn validate_recipient(text: &str, role: Option<&str>) -> Result<(), String> {
    validate_controls(text)?;
    if text.contains(['\t', '\n', '\r']) && role != Some("AXTextArea") {
        return Err("type inserts tab/newline/return only into a multi-line text area; use key with `[\"tab\"]` or `[\"return\"]` to move focus or submit".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_controls_need_an_explicit_key_action() {
        for c in (0u8..=31)
            .filter(|c| !matches!(c, 9 | 10 | 13))
            .chain([127])
        {
            let failure = validate_controls(&format!("a{}z", char::from(c))).unwrap_err();
            assert!(failure.contains("key"));
        }
        assert!(validate_controls("a\tB\n/🦀\r\n").is_ok());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn only_multiline_recipients_allow_tab_newline_and_return() {
        for text in ["user\tpass", "user\npass", "user\rpass", "user\r\npass"] {
            for role in [
                None,
                Some("AXTextField"),
                Some("AXComboBox"),
                Some("AXButton"),
            ] {
                let failure = validate_recipient(text, role).unwrap_err();
                assert!(failure.contains("[\"tab\"]"));
                assert!(failure.contains("[\"return\"]"));
            }
            assert!(validate_recipient(text, Some("AXTextArea")).is_ok());
        }
        assert!(validate_recipient("ordinary 🦀", Some("AXTextField")).is_ok());
    }
}
