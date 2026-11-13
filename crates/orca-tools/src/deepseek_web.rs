//! DeepSeek's Search and Fetch APIs, which `web_search` and `web_fetch` use
//! when Orca talks to the official DeepSeek endpoint.

use std::fmt;

use orca_core::config::ProviderKind;

/// The only origin the web tools ever send the DeepSeek API key to.
const DEEPSEEK_ORIGIN: &str = "https://api.deepseek.com";
const DEEPSEEK_HOST: &str = "api.deepseek.com";

/// Permission for the web tools to call DeepSeek's web APIs with the
/// session's API key.
#[derive(Clone)]
pub struct DeepSeekWebAccess {
    origin: String,
    // Read by the search and fetch requests, which land with the DeepSeek
    // backends in the next change.
    #[allow(dead_code)]
    api_key: String,
}

impl DeepSeekWebAccess {
    /// The access a session gets, or `None` when its tools must not call
    /// DeepSeek: the switch is off, the provider is not DeepSeek, there is no
    /// key, or the endpoint is anything other than the official one over
    /// https on the default port. A custom endpoint usually carries another
    /// provider's key, and that key must never reach DeepSeek.
    pub fn for_provider(
        provider: ProviderKind,
        base_url: Option<&str>,
        api_key: Option<&str>,
        enabled: bool,
    ) -> Option<Self> {
        if !enabled || provider != ProviderKind::DeepSeek {
            return None;
        }
        let api_key = api_key.map(str::trim).filter(|key| !key.is_empty())?;
        if base_url.is_some_and(|base_url| !is_official_endpoint(base_url)) {
            return None;
        }
        Some(Self {
            origin: DEEPSEEK_ORIGIN.to_string(),
            api_key: api_key.to_string(),
        })
    }
}

fn is_official_endpoint(base_url: &str) -> bool {
    url::Url::parse(base_url.trim()).is_ok_and(|url| {
        url.scheme() == "https"
            && url
                .host_str()
                .is_some_and(|host| host.eq_ignore_ascii_case(DEEPSEEK_HOST))
            && url.port().is_none()
    })
}

impl fmt::Debug for DeepSeekWebAccess {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeepSeekWebAccess")
            .field("origin", &self.origin)
            .field("api_key", &"<redacted>")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::config::ProviderKind;

    fn for_provider(
        provider: ProviderKind,
        base_url: Option<&str>,
        api_key: Option<&str>,
        enabled: bool,
    ) -> Option<DeepSeekWebAccess> {
        DeepSeekWebAccess::for_provider(provider, base_url, api_key, enabled)
    }

    #[test]
    fn access_requires_official_https_endpoint_and_a_key() {
        let key = Some("sk-x");
        assert!(for_provider(ProviderKind::DeepSeek, None, key, true).is_some());
        for allowed in ["https://api.deepseek.com/beta", "https://API.DeepSeek.com"] {
            assert!(
                for_provider(ProviderKind::DeepSeek, Some(allowed), key, true).is_some(),
                "{allowed} should be allowed"
            );
        }
        for refused in [
            "http://api.deepseek.com",
            "https://api.deepseek.com.evil.example",
            "https://api.deepseek.com:8443",
            "https://api.deepseek.com@evil.example/",
            "https://api.siliconflow.cn/v1",
        ] {
            assert!(
                for_provider(ProviderKind::DeepSeek, Some(refused), key, true).is_none(),
                "{refused} should be refused"
            );
        }
        assert!(for_provider(ProviderKind::DeepSeek, None, None, true).is_none());
        assert!(for_provider(ProviderKind::DeepSeek, None, Some("  "), true).is_none());
        assert!(for_provider(ProviderKind::DeepSeek, None, key, false).is_none());
        assert!(for_provider(ProviderKind::Mock, None, key, true).is_none());
    }

    #[test]
    fn access_debug_redacts_the_key() {
        let access = for_provider(ProviderKind::DeepSeek, None, Some("sk-x"), true)
            .expect("official endpoint access");
        let debug = format!("{access:?}");

        assert!(!debug.contains("sk-x"), "{debug}");
        assert!(debug.contains("<redacted>"), "{debug}");
    }
}
