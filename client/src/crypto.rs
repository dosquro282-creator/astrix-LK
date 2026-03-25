use rand::RngCore;
use serde::{Deserialize, Serialize};

/// Длина ключа для ChaCha20-Poly1305.
pub const KEY_LEN: usize = 32;
pub const NONCE_LEN: usize = 12;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelKey {
    /// Симметричный ключ канала.
    pub key: [u8; KEY_LEN],
}

#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("encryption error")]
    Encrypt,
    #[error("decryption error")]
    Decrypt,
}

/// Генерация нового ключа канала.
pub fn generate_channel_key() -> ChannelKey {
    let mut key = [0u8; KEY_LEN];
    rand::thread_rng().fill_bytes(&mut key);
    ChannelKey { key }
}

/// Шифрование текста в формате ciphertext + nonce.
pub fn encrypt_message(key: &ChannelKey, plaintext: &str) -> Result<(Vec<u8>, [u8; NONCE_LEN]), CryptoError> {
    use chacha20poly1305::{
        aead::{Aead, KeyInit},
        ChaCha20Poly1305, Key, Nonce,
    };

    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);

    let cipher = ChaCha20Poly1305::new(&Key::from_slice(&key.key));
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ciphertext = cipher
        .encrypt(nonce, plaintext.as_bytes())
        .map_err(|_| CryptoError::Encrypt)?;
    Ok((ciphertext, nonce_bytes))
}

/// Расшифровка текста по ключу и nonce.
pub fn decrypt_message(
    key: &ChannelKey,
    ciphertext: &[u8],
    nonce_bytes: &[u8; NONCE_LEN],
) -> Result<String, CryptoError> {
    use chacha20poly1305::{
        aead::{Aead, KeyInit},
        ChaCha20Poly1305, Key, Nonce,
    };

    let cipher = ChaCha20Poly1305::new(&Key::from_slice(&key.key));
    let nonce = Nonce::from_slice(nonce_bytes);
    let plaintext = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|_| CryptoError::Decrypt)?;
    String::from_utf8(plaintext).map_err(|_| CryptoError::Decrypt)
}

