use sha2::{Digest, Sha256};

pub fn command_hash(command: &str) -> String {
    hash_value(command)
}

pub fn chat_id_hash(chat_id: &str) -> String {
    hash_value(chat_id)
}

pub fn command_preview(command: &str, max_chars: usize) -> String {
    let redacted = redact_secrets(command);
    let safe = stop_at_blob(&redacted);
    safe.chars().take(max_chars).collect()
}

pub fn redact_secrets(input: &str) -> String {
    let mut out = Vec::new();
    let mut redact_next = false;

    for token in input.split_whitespace() {
        if redact_next {
            out.push("<redacted>".to_string());
            redact_next = false;
        } else if is_bearer_marker(token) {
            out.push("<redacted>".to_string());
            redact_next = true;
        } else if is_secret_token(token) {
            out.push("<redacted>".to_string());
        } else if let Some((key, value)) = token.split_once('=') {
            if is_secret_key(key) || is_secret_token(value) || is_bearer_marker(value) {
                out.push(format!("{key}=<redacted>"));
                if is_bearer_marker(value) {
                    redact_next = true;
                }
            } else {
                out.push(token.to_string());
            }
        } else {
            out.push(token.to_string());
        }
    }

    let mut joined = out.join(" ");
    joined = redact_after_marker(&joined, "Bearer ");
    joined = redact_after_marker(&joined, "bearer ");
    joined
}

fn hash_value(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    format!("sha256:{}", hex::encode(digest))
}

fn redact_after_marker(input: &str, marker: &str) -> String {
    let mut remaining = input;
    let mut out = String::new();

    while let Some(idx) = remaining.find(marker) {
        out.push_str(&remaining[..idx + marker.len()]);
        out.push_str("<redacted>");
        let after = &remaining[idx + marker.len()..];
        let end = after.find(char::is_whitespace).unwrap_or(after.len());
        remaining = &after[end..];
    }

    out.push_str(remaining);
    out
}

fn stop_at_blob(input: &str) -> &str {
    for (idx, token) in input.split_whitespace().enumerate() {
        if token.len() >= 32 && token.bytes().all(is_base64ish) {
            let byte_idx = input
                .split_whitespace()
                .take(idx)
                .map(|part| part.len() + 1)
                .sum::<usize>();
            return input[..byte_idx.saturating_sub(1)].trim_end();
        }
    }

    input
}

fn is_base64ish(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'=' | b'_' | b'-' | b'.')
}

fn is_secret_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    lower.contains("token")
        || lower.contains("api_key")
        || lower.contains("apikey")
        || lower.contains("secret")
        || lower.contains("password")
}

fn is_bearer_marker(token: &str) -> bool {
    token
        .trim_matches(|ch: char| matches!(ch, '"' | '\'' | ':' | '='))
        .eq_ignore_ascii_case("bearer")
}

fn is_secret_token(token: &str) -> bool {
    let trimmed = token.trim_matches(|ch: char| {
        matches!(
            ch,
            '"' | '\'' | ',' | ';' | ')' | '(' | '[' | ']' | '{' | '}'
        )
    });
    let lower = trimmed.to_ascii_lowercase();

    lower.starts_with("env:")
        || lower.starts_with("bearer ")
        || trimmed.starts_with("sk-") && trimmed.len() > 12
        || looks_like_telegram_token(trimmed)
        || looks_like_jwt(trimmed)
}

fn looks_like_telegram_token(token: &str) -> bool {
    let Some((left, right)) = token.split_once(':') else {
        return false;
    };

    left.len() >= 6
        && left.bytes().all(|byte| byte.is_ascii_digit())
        && right.len() >= 8
        && right.bytes().all(is_base64ish)
}

fn looks_like_jwt(token: &str) -> bool {
    let mut parts = token.split('.');
    let first = parts.next();
    let second = parts.next();
    let third = parts.next();
    parts.next().is_none()
        && first.is_some_and(|part| part.len() >= 3 && part.bytes().all(is_base64ish))
        && second.is_some_and(|part| part.len() >= 3 && part.bytes().all(is_base64ish))
        && third.is_some_and(|part| part.len() >= 3 && part.bytes().all(is_base64ish))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_are_deterministic_and_prefixed() {
        assert_eq!(command_hash("git status"), command_hash("git status"));
        assert_ne!(command_hash("git status"), command_hash("git reset --hard"));
        assert!(command_hash("git status").starts_with("sha256:"));
        assert_eq!(chat_id_hash("123456"), chat_id_hash("123456"));
    }

    #[test]
    fn preview_redacts_and_bounds_plaintext() {
        let command = "curl -H 'Authorization: Bearer secret-token-value' https://example.test/very/long/path";
        let preview = command_preview(command, 40);

        assert!(preview.chars().count() <= 40);
        assert!(!preview.contains("secret-token-value"));
        assert!(preview.contains("<redacted>"));
    }

    #[test]
    fn redaction_removes_common_secret_shapes() {
        let input = "bot=123456:ABCdef_TOKEN api_key=sk-abcdefghijklmnopqrstuvwxyz bearer=Bearer aaa.bbb.ccc env:TELEGRAM_BOT_TOKEN";
        let redacted = redact_secrets(input);

        assert!(!redacted.contains("123456:ABCdef_TOKEN"));
        assert!(!redacted.contains("sk-abcdefghijklmnopqrstuvwxyz"));
        assert!(!redacted.contains("aaa.bbb.ccc"));
        assert!(!redacted.contains("TELEGRAM_BOT_TOKEN"));
        assert!(redacted.contains("<redacted>"));
    }
}
