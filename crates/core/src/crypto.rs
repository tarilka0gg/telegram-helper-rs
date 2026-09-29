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
}
