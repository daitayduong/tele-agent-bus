use std::path::Path;
use std::fs;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use thiserror::Error;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Error)]
pub enum IntegrityError {
    #[error("hmac signature mismatch — file tampered or key changed")]
    Mismatch,
    #[error("missing signature file: {0}")]
    MissingSignature(String),
    #[error("invalid hex in signature: {0}")]
    InvalidHex(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub fn compute_hmac(key: &[u8], body: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC can take key of any size");
    mac.update(body);
    let result = mac.finalize();
    hex::encode(result.into_bytes())
}

pub fn verify_hmac(key: &[u8], body: &[u8], hex_sig: &str) -> Result<(), IntegrityError> {
    let sig = hex::decode(hex_sig.trim())
        .map_err(|e| IntegrityError::InvalidHex(e.to_string()))?;
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC can take key of any size");
    mac.update(body);
    mac.verify_slice(&sig).map_err(|_| IntegrityError::Mismatch)
}

pub fn load_and_verify(
    conf_path: &Path,
    hmac_path: &Path,
    key_path: &Path,
) -> Result<Vec<String>, IntegrityError> {
    let key = fs::read(key_path)?;
    let body = fs::read(conf_path)?;
    let hex_sig = fs::read_to_string(hmac_path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            IntegrityError::MissingSignature(hmac_path.to_string_lossy().into_owned())
        } else {
            IntegrityError::Io(e)
        }
    })?;

    verify_hmac(&key, &body, &hex_sig)?;

    let content = String::from_utf8_lossy(&body);
    let patterns = content
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    Ok(patterns)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_compute_verify_roundtrip() {
        let key = b"secret";
        let body = b"hello world";
        let sig = compute_hmac(key, body);
        assert!(verify_hmac(key, body, &sig).is_ok());
    }

    #[test]
    fn test_verify_mismatch() {
        let key = b"secret";
        let body = b"hello world";
        let sig = compute_hmac(key, body);
        assert!(matches!(verify_hmac(key, b"tampered", &sig), Err(IntegrityError::Mismatch)));
    }

    #[test]
    fn test_load_and_verify_happy_path() -> Result<(), Box<dyn std::error::Error>> {
        let key = b"01234567890123456789012345678901";
        let body = b"^rm -rf\n^ls -R";
        let sig = compute_hmac(key, body);

        let mut kf = NamedTempFile::new()?;
        kf.write_all(key)?;
        let mut cf = NamedTempFile::new()?;
        cf.write_all(body)?;
        let mut hf = NamedTempFile::new()?;
        hf.write_all(sig.as_bytes())?;

        let patterns = load_and_verify(cf.path(), hf.path(), kf.path())?;
        assert_eq!(patterns, vec!["^rm -rf", "^ls -R"]);
        Ok(())
    }

    #[test]
    fn test_load_and_verify_tampered() -> Result<(), Box<dyn std::error::Error>> {
        let key = b"01234567890123456789012345678901";
        let body = b"clean";
        let sig = compute_hmac(key, body);

        let mut kf = NamedTempFile::new()?;
        kf.write_all(key)?;
        let mut cf = NamedTempFile::new()?;
        cf.write_all(b"tampered")?;
        let mut hf = NamedTempFile::new()?;
        hf.write_all(sig.as_bytes())?;

        let res = load_and_verify(cf.path(), hf.path(), kf.path());
        assert!(matches!(res, Err(IntegrityError::Mismatch)));
        Ok(())
    }

    #[test]
    fn test_missing_signature() {
        let kf = NamedTempFile::new().unwrap();
        let cf = NamedTempFile::new().unwrap();
        let hf_path = Path::new("/tmp/definitely_not_there_12345");

        let res = load_and_verify(cf.path(), hf_path, kf.path());
        assert!(matches!(res, Err(IntegrityError::MissingSignature(_))));
    }

    #[test]
    fn test_invalid_hex() {
        let res = verify_hmac(b"key", b"body", "not hex");
        assert!(matches!(res, Err(IntegrityError::InvalidHex(_))));
    }
}
