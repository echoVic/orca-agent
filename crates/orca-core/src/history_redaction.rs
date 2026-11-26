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

fn redact_plain_text(value: &str) -> String {
    redact_standalone_secret_tokens(&redact_keyed_secret_values(value))
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

pub fn redact_keyed_secret_values(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = String::with_capacity(value.len());
    let mut cursor = 0;
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'=' && bytes[index] != b':' {
            index += 1;
            continue;
        }

        let key_start = key_start_before_delimiter(bytes, index);
        let key = trim_key_candidate(&value[key_start..index]);
        if !is_sensitive_key(key) {
            index += 1;
            continue;
        }

        let mut value_start = index + 1;
        while value_start < bytes.len() && bytes[value_start].is_ascii_whitespace() {
            value_start += 1;
        }
        let quote = (value_start < bytes.len()
            && (bytes[value_start] == b'"' || bytes[value_start] == b'\''))
            .then(|| bytes[value_start]);
        let content_start = if quote.is_some() {
            value_start + 1
        } else {
            value_start
        };
        let content_end = if let Some(quote) = quote {
            quoted_value_end(bytes, content_start, quote)
        } else {
            unquoted_value_end(
                bytes,
                content_start,
                key_is_in_url_query(&value[..key_start]),
            )
        };
        if content_start == content_end {
            index += 1;
            continue;
        }

        output.push_str(&value[cursor..content_start]);
        output.push_str("<redacted>");
        if quote.is_some() && content_end < bytes.len() {
            output.push(bytes[content_end] as char);
            cursor = content_end + 1;
            index = cursor;
        } else {
            cursor = content_end;
            index = cursor;
        }
    }
    output.push_str(&value[cursor..]);
    output
}

fn key_start_before_delimiter(bytes: &[u8], delimiter_index: usize) -> usize {
    let mut start = delimiter_index;
    while start > 0 {
        let previous = bytes[start - 1];
        if previous.is_ascii_whitespace()
            || matches!(previous, b'{' | b'[' | b',' | b';' | b'(' | b'?' | b'&')
        {
            break;
        }
        start -= 1;
    }
    start
}

fn trim_key_candidate(key: &str) -> &str {
    key.trim_matches(|ch: char| ch.is_whitespace() || matches!(ch, '"' | '\'' | '{' | '[' | ','))
}

fn is_sensitive_key(key: &str) -> bool {
    // A key is a config, env, or JSON name. A path or a code span before a
    // colon (`auth/token.py`: …) is prose naming a file, not a credential.
    if key.is_empty()
        || !key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'$'))
    {
        return false;
    }
    let key = key.to_ascii_lowercase();
    key.contains("api_key")
        || key.contains("apikey")
        || key.contains("token")
        || key.contains("password")
        || key.contains("secret")
        || key.contains("authorization")
}

fn quoted_value_end(bytes: &[u8], mut index: usize, quote: u8) -> usize {
    let mut escaped = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if escaped {
            escaped = false;
        } else if byte == b'\\' {
            escaped = true;
        } else if byte == quote {
            return index;
        }
        index += 1;
    }
    index
}

fn key_is_in_url_query(prefix: &str) -> bool {
    let token = prefix
        .rsplit(|ch: char| {
            ch.is_whitespace() || matches!(ch, '\"' | '\'' | '{' | '[' | '(' | ',' | ';')
        })
        .next()
        .unwrap_or(prefix);
    let Some(scheme_end) = token.find("://") else {
        return false;
    };
    let Some(query_start) = token.find('?') else {
        return false;
    };
    scheme_end < query_start && !token.contains('#')
}

fn unquoted_value_end(bytes: &[u8], mut index: usize, url_query: bool) -> usize {
    while index < bytes.len()
        && !bytes[index].is_ascii_whitespace()
        && !matches!(bytes[index], b',' | b'}' | b']' | b';')
        && !(url_query && matches!(bytes[index], b'&' | b'#'))
    {
        index += 1;
    }
    index
}

fn redact_standalone_secret_tokens(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut token = String::new();
    for ch in value.chars() {
        if is_secret_token_boundary(ch) {
            push_redacted_token(&mut output, &mut token);
            output.push(ch);
        } else {
            token.push(ch);
        }
    }
    push_redacted_token(&mut output, &mut token);
    output
}

fn is_secret_token_boundary(ch: char) -> bool {
    ch.is_whitespace() || matches!(ch, '"' | '\'' | '{' | '}' | '[' | ']' | ',' | ':')
}

fn push_redacted_token(output: &mut String, token: &mut String) {
    if token.is_empty() {
        return;
    }
    if looks_like_standalone_secret(token) {
        output.push_str("<redacted>");
    } else {
        output.push_str(token);
    }
    token.clear();
}

fn looks_like_standalone_secret(token: &str) -> bool {
    let trimmed = token.trim_matches(|ch: char| {
        matches!(
            ch,
            '.' | ';' | ')' | '(' | '<' | '>' | '`' | '"' | '\'' | '='
        )
    });
    let lower = trimmed.to_ascii_lowercase();
    lower.starts_with("sk-")
        || (trimmed.len() >= 16
            && lower.contains("secret")
            && (lower.contains("token")
                || lower.contains("password")
                || lower.contains("key")
                || lower.contains("test")))
}
