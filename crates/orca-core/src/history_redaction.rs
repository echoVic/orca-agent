use crate::redaction::{is_sensitive_key, redact_sensitive_text as redact_plain_text};

pub fn redact_sensitive_text(value: &str) -> String {
    // Structured tool output can carry multiline text inside a JSON string.
    // Redact its decoded value so escaped line breaks act like ordinary ones.
    if value.contains('\\')
        && let Ok(mut json) = serde_json::from_str(value)
    {
        return if redact_json_strings(&mut json) {
            json.to_string()
        } else {
            value.to_owned()
        };
    }
    redact_plain_text(value)
}

fn redact_json_strings(value: &mut serde_json::Value) -> bool {
    use serde_json::Value;
    match value {
        Value::String(text) => {
            let redacted = redact_plain_text(text);
            let changed = *text != redacted;
            *text = redacted;
            changed
        }
        Value::Array(items) => items
            .iter_mut()
            .map(redact_json_strings)
            .fold(false, |changed, item| changed | item),
        Value::Object(fields) => fields.iter_mut().fold(false, |changed, (name, value)| {
            if is_sensitive_key(name)
                && value
                    .as_str()
                    .is_some_and(|text| !text.is_empty() && text != "<redacted>")
            {
                *value = Value::String("<redacted>".to_owned());
                true
            } else {
                redact_json_strings(value) | changed
            }
        }),
        _ => false,
    }
}
