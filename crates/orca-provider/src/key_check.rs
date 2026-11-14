//! Checks an API key against the provider's free `GET /models` endpoint.

use std::time::Duration;

const DEEPSEEK_HOST: &str = "api.deepseek.com";

/// What the provider said about a key.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KeyCheck {
    /// The provider listed its models for this key.
    Valid,
    /// The provider refused the key (HTTP 401 or 403).
    Rejected(u16),
    /// Some other status, which says nothing about the key.
    Unexpected(u16),
    /// No answer, and why.
    Unreachable(String),
}

/// The models list next to the chat endpoint at `base_url`, the official
/// DeepSeek endpoint when it is `None`. DeepSeek serves it only at its root,
/// so a path there such as `/beta` is dropped; any other endpoint gets
/// `/models` after its path, the OpenAI-compatible layout.
pub fn models_url(base_url: Option<&str>) -> String {
    let base_url = base_url
        .map(|url| url.trim().trim_end_matches('/'))
        .filter(|url| !url.is_empty())
        .unwrap_or("https://api.deepseek.com");
    if let Ok(mut url) = reqwest::Url::parse(base_url)
        && url
            .host_str()
            .is_some_and(|host| host.eq_ignore_ascii_case(DEEPSEEK_HOST))
    {
        url.set_path("/models");
        url.set_query(None);
        url.set_fragment(None);
        return url.into();
    }
    format!("{base_url}/models")
}

/// Asks the endpoint at `base_url` to list its models with `api_key`. The
/// request is free: it reaches no model. The key is sent as it is, as the
/// provider sends it, and a redirect is not followed.
pub fn check_api_key(base_url: Option<&str>, api_key: &str, timeout: Duration) -> KeyCheck {
    let builder = orca_mcp::http::blocking_client_builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none());
    // Test fixtures listen on loopback, which a proxy cannot reach.
    #[cfg(test)]
    let builder = builder.no_proxy();
    let client = match builder.build() {
        Ok(client) => client,
        Err(error) => return KeyCheck::Unreachable(orca_mcp::http::build_error(&error)),
    };
    match client.get(models_url(base_url)).bearer_auth(api_key).send() {
        Ok(response) => match response.status().as_u16() {
            200..=299 => KeyCheck::Valid,
            status @ (401 | 403) => KeyCheck::Rejected(status),
            status => KeyCheck::Unexpected(status),
        },
        Err(error) => KeyCheck::Unreachable(describe(error.without_url())),
    }
}

/// `error` followed by each of its sources: reqwest's own message alone
/// leaves out why the request failed.
fn describe(error: reqwest::Error) -> String {
    if error.is_timeout() {
        return "the request timed out".to_string();
    }
    let mut text = error.to_string();
    let mut source = std::error::Error::source(&error);
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    /// Answers one request with `status` and returns the raw request.
    fn serve_once(status: u16) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind models fixture");
        let base = format!("http://{}", listener.local_addr().expect("fixture address"));
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept models request");
            stream
                .set_nonblocking(false)
                .expect("blocking models stream");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("read timeout");
            let mut request = [0_u8; 2048];
            let read = stream.read(&mut request).unwrap_or(0);
            let body = r#"{"object":"list","data":[{"id":"deepseek-flash","object":"model"}]}"#;
            let reply = format!(
                "HTTP/1.1 {status} Fixture\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(reply.as_bytes());
            String::from_utf8_lossy(&request[..read]).into_owned()
        });
        (base, handle)
    }

    #[test]
    fn models_url_uses_the_official_origin_or_the_configured_base() {
        assert_eq!(models_url(None), "https://api.deepseek.com/models");
        assert_eq!(
            models_url(Some("https://api.deepseek.com/beta")),
            "https://api.deepseek.com/models"
        );
        assert_eq!(
            models_url(Some("https://api.siliconflow.cn/v1/")),
            "https://api.siliconflow.cn/v1/models"
        );
    }

    #[test]
    fn key_check_reads_the_models_status() {
        for (status, expected) in [
            (200, KeyCheck::Valid),
            (401, KeyCheck::Rejected(401)),
            (403, KeyCheck::Rejected(403)),
            (500, KeyCheck::Unexpected(500)),
        ] {
            let (base, server) = serve_once(status);

            let outcome = check_api_key(Some(&base), "sk-test", Duration::from_secs(5));

            assert_eq!(outcome, expected, "HTTP {status}");
            let request = server.join().expect("models fixture");
            assert!(request.starts_with("GET /models HTTP/1.1\r\n"), "{request}");
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("\r\nauthorization: bearer sk-test\r\n"),
                "{request}"
            );
        }
    }

    #[test]
    fn key_check_reports_an_unreachable_endpoint() {
        // Nothing listens on the discard port.
        let outcome = check_api_key(
            Some("http://127.0.0.1:9"),
            "sk-test",
            Duration::from_secs(5),
        );

        assert!(matches!(outcome, KeyCheck::Unreachable(_)), "{outcome:?}");
    }
}
