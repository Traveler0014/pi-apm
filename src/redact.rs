//! Shared token redaction for everything that leaves the process.
//!
//! Two surfaces need the same scrub: MCP tool results (model-facing) and the
//! NDJSON export (archival, may be pasted into an issue). Both used to carry
//! their own regex-ish matcher, which is the kind of duplication that lets one
//! side fall behind when a new token shape is introduced.
//!
//! A token is `apm_` followed by at least 32 characters from `[A-Za-z0-9_-]`.
//! That covers every shape this project issues today:
//!
//! * claim tokens — `apm_` + two UUIDs (64 hex chars);
//! * REST bearer tokens — `apm_rest_` + hex;
//! * UI session tokens — `apm_ui_` + hex;
//! * bootstrap tickets — `apm_boot_` + hex.
//!
//! Tool *names* like `apm_dashboard` stay readable: they are shorter than the
//! 32-character body, and the documentation is full of them.

/// Prefix every APM secret starts with.
pub const TOKEN_PREFIX: &str = "apm_";
/// Minimum body length before an `apm_`-prefixed word is treated as a secret.
pub const MIN_TOKEN_BODY: usize = 32;
/// What a matched token is replaced with.
pub const REPLACEMENT: &str = "apm_[redacted]";

/// Replace any token-shaped run in `text` with [`REPLACEMENT`].
///
/// Best effort only: this is a belt-and-braces scrub for accidental leaks, not a
/// general-purpose secret scanner (see docs/concepts.md).
pub fn redact_tokens(text: &str) -> String {
    if !text.contains(TOKEN_PREFIX) {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let Some(position) = rest.find(TOKEN_PREFIX) else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..position]);
        let after = &rest[position + TOKEN_PREFIX.len()..];
        let body_len = after
            .bytes()
            .take_while(|byte| byte.is_ascii_alphanumeric() || *byte == b'_' || *byte == b'-')
            .count();
        if body_len >= MIN_TOKEN_BODY {
            out.push_str(REPLACEMENT);
            rest = &after[body_len..];
        } else {
            out.push_str(TOKEN_PREFIX);
            rest = after;
        }
    }
    out
}

/// Redact every string inside a JSON value, leaving keys and structure alone.
pub fn redact_value(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::String(text) => serde_json::Value::String(redact_tokens(&text)),
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(redact_value).collect())
        }
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.into_iter()
                .map(|(key, value)| (key, redact_value(value)))
                .collect(),
        ),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claim_tokens_are_redacted() {
        let token = format!("apm_{}", "ab".repeat(32));
        assert_eq!(
            redact_tokens(&format!("token {token} leaked")),
            "token apm_[redacted] leaked"
        );
    }

    #[test]
    fn every_prefixed_shape_is_covered() {
        for prefix in ["apm_", "apm_rest_", "apm_ui_", "apm_boot_"] {
            let token = format!("{prefix}{}", "0a1b".repeat(12));
            assert!(token.len() - 4 >= MIN_TOKEN_BODY, "{token}");
            let redacted = redact_tokens(&format!("[{token}]"));
            assert_eq!(redacted, "[apm_[redacted]]", "shape {prefix}");
            assert!(!redacted.contains(&token));
        }
    }

    #[test]
    fn words_and_short_bodies_stay_readable() {
        assert_eq!(
            redact_tokens("apm_1234 and apm_dashboard"),
            "apm_1234 and apm_dashboard"
        );
        assert_eq!(redact_tokens("no tokens here"), "no tokens here");
        // A 31-character body is under the threshold; 32 is a token.
        let short = format!("apm_{}", "a".repeat(MIN_TOKEN_BODY - 1));
        assert_eq!(redact_tokens(&short), short);
        let exact = format!("apm_{}", "a".repeat(MIN_TOKEN_BODY));
        assert_eq!(redact_tokens(&exact), REPLACEMENT);
    }

    #[test]
    fn only_strings_are_scrubbed_in_values() {
        let token = format!("apm_{}", "cd".repeat(20));
        let value = serde_json::json!({
            "token": token,
            "nested": [format!("prefix {token}"), 7, true],
            "apm_key": "kept",
        });
        let redacted = redact_value(value);
        assert_eq!(redacted["token"], serde_json::json!(REPLACEMENT));
        assert_eq!(
            redacted["nested"][0],
            serde_json::json!("prefix apm_[redacted]")
        );
        assert_eq!(redacted["nested"][1], serde_json::json!(7));
        assert_eq!(redacted["apm_key"], serde_json::json!("kept"));
        assert!(!redacted.to_string().contains(&token));
    }
}
