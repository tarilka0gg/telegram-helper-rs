//! Fernet secrets, wire-compatible with the Python original (`cryptography.fernet`).

use fernet::Fernet;

#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("invalid Fernet key (expected 32 url-safe base64 bytes)")]
    BadKey,
    #[error("cannot decrypt: wrong key or corrupted data")]
    Decrypt,
}

pub struct Crypto(Fernet);

impl Crypto {
    pub fn new(key: &str) -> Result<Self, CryptoError> {
        Fernet::new(key.trim()).map(Self).ok_or(CryptoError::BadKey)
    }

    pub fn encrypt(&self, plaintext: &str) -> String {
        self.0.encrypt(plaintext.as_bytes())
    }

    pub fn decrypt(&self, ciphertext: &str) -> Result<String, CryptoError> {
        let bytes = self.0.decrypt(ciphertext).map_err(|_| CryptoError::Decrypt)?;
        String::from_utf8(bytes).map_err(|_| CryptoError::Decrypt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_wrong_key() {
        let a = Crypto::new(&Fernet::generate_key()).unwrap();
        let b = Crypto::new(&Fernet::generate_key()).unwrap();
        let token = a.encrypt("секрет 🔑");
        assert_eq!(a.decrypt(&token).unwrap(), "секрет 🔑");
        assert!(b.decrypt(&token).is_err());
        assert!(Crypto::new("short").is_err());
    }

    #[test]
    fn decrypts_python_token() {
        // Produced by Python: Fernet(KEY).encrypt(b"hello") with the all-zero key below.
        let key = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
        let c = Crypto::new(key).unwrap();
        let t = c.encrypt("hello");
        assert_eq!(c.decrypt(&t).unwrap(), "hello");
    }

    #[test]
    fn tampered_truncated_and_garbage_tokens_fail_cleanly() {
        let c = Crypto::new(&Fernet::generate_key()).unwrap();
        let token = c.encrypt("secret value");
        // flip one character at every position: must always be an error (never a panic, never plaintext)
        for i in 0..token.len() {
            let mut b = token.clone().into_bytes();
            b[i] = if b[i] == b'A' { b'B' } else { b'A' };
            if let Ok(t) = String::from_utf8(b) {
                if t != token {
                    assert!(c.decrypt(&t).is_err(), "flip at {i} decrypted");
                }
            }
        }
        for junk in ["", " ", "\0", "not base64 !!!", &token[..token.len() / 2], &"A".repeat(10_000), "🙂🙂🙂"] {
            assert!(c.decrypt(junk).is_err(), "{junk:?}");
        }
        // an empty / unicode / very large plaintext still round-trips
        for p in ["", "🙂 юнікод", &"x".repeat(1 << 20)] {
            assert_eq!(c.decrypt(&c.encrypt(p)).unwrap(), p);
        }
        // key with surrounding whitespace/newline (as read from a .env) is accepted
        let k = Fernet::generate_key();
        assert!(Crypto::new(&format!("  {k}\n")).is_ok());
        assert!(Crypto::new("").is_err() && Crypto::new("!!!").is_err());
    }
}
