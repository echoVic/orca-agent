//! Money formatting.

/// Format cents as `$12.34` (`-` in front for negatives).
pub fn format_cents(cents: i64) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    let absolute = cents.abs();
    format!("{sign}${}.{:02}", absolute / 100, absolute % 100)
}
