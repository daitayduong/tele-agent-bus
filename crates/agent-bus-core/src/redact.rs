use thiserror::Error;

#[derive(Debug, Error)]
pub enum RedactError {}

pub fn command_hash(_command: &str) -> String {
    todo!("RED: implemented after tests")
}

pub fn chat_id_hash(_chat_id: &str) -> String {
    todo!("RED: implemented after tests")
}

pub fn command_preview(_command: &str, _max_chars: usize) -> String {
    todo!("RED: implemented after tests")
}

pub fn redact_secrets(_input: &str) -> String {
    todo!("RED: implemented after tests")
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
