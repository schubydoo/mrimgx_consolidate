//! Key derivation, the password test and the per-block initialization vector.
//!
//! None of this is needed to consolidate a set. A merge copies stored bytes, so it never
//! decrypts anything and never asks for a password. This module exists for the optional
//! end-to-end test, which decrypts every copied block of an output and compares its
//! plaintext hash against the hash the index records.
//!
//! Every algorithm here is fixed by the file format. None of them is a design choice and
//! none can be substituted. `scratch/format-notes.md` records where each one comes from.

use aes::cipher::block::BlockCipherEncrypt;
use aes::cipher::block_padding::NoPadding;
use aes::cipher::{BlockModeDecrypt, KeyInit, KeyIvInit};
use anyhow::{bail, ensure, Result};
use sha2::{Digest, Sha256};

/// The derived key is always 32 bytes, whatever `aes_type` the payload uses.
pub const KEY_LEN: usize = 32;

/// The AES block size. Every stored length is a multiple of it when encryption is on.
pub const AES_BLOCK: usize = 16;

/// Derive the key from a password.
///
/// The salt is the SHA-256 of the eight raw bytes behind `imageid`, not of the text form.
/// `iterations` comes from `_encryption.key_iterations`, which is 600000 in every file seen.
pub fn derive_key(password: &str, imageid: [u8; 8], iterations: u32) -> [u8; KEY_LEN] {
    let salt = Sha256::digest(imageid);
    let mut key = [0u8; KEY_LEN];
    pbkdf2::pbkdf2_hmac::<Sha256>(password.as_bytes(), &salt, iterations, &mut key);
    key
}

/// The password test: HMAC-SHA256 of the empty message, keyed with the derived key.
///
/// The result is compared against `_encryption.hmac`, which is 64 hex characters. The
/// example in the published `ENCRYPTION.md` shows 63, which is a typo in that document.
pub fn password_hmac(key: &[u8; KEY_LEN]) -> [u8; 32] {
    use hmac::Mac;
    let mut mac = hmac::Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes any key length");
    mac.update(b"");
    mac.finalize().into_bytes().into()
}

/// Make sure that the password is the one the set was made with.
pub fn check_password(key: &[u8; KEY_LEN], expected: &str) -> Result<()> {
    let expected = expected.trim();
    ensure!(
        expected.len() == 64,
        "_encryption.hmac is {} characters, expected 64",
        expected.len()
    );
    let got = password_hmac(key);
    let got_hex: String = got.iter().map(|b| format!("{b:02x}")).collect();
    ensure!(
        got_hex.eq_ignore_ascii_case(expected),
        "the password is wrong for this backup set"
    );
    Ok(())
}

/// The initialization vector of one block.
///
/// The scheme is ESSIV: a 16-byte tuple describing where the block sits, encrypted with
/// AES-256-ECB under the SHA-256 of the derived key. The initialization vector cipher is
/// always AES-256, whatever the payload uses.
///
/// `block_index` is the position of the block in the flattened array of its partition.
/// Reserved sector blocks count from zero in their own array, so a reserved block and a data
/// block at the same position share an initialization vector. The reference restore does
/// exactly this.
pub fn block_iv(
    key: &[u8; KEY_LEN],
    imageid: [u8; 8],
    disk: u16,
    partition: u16,
    block_index: u32,
) -> [u8; 16] {
    let mut plain = [0u8; 16];
    plain[0..8].copy_from_slice(&imageid);
    plain[8..10].copy_from_slice(&disk.to_le_bytes());
    plain[10..12].copy_from_slice(&partition.to_le_bytes());
    plain[12..16].copy_from_slice(&block_index.to_le_bytes());

    let iv_key = Sha256::digest(key);
    let cipher = aes::Aes256::new(&iv_key);
    let mut block = aes::cipher::Array(plain);
    cipher.encrypt_block(&mut block);
    block.0
}

/// How many bytes of the derived key one `aes_type` uses.
///
/// The names map to the OpenSSL round counts of 10, 12 and 14. The key is the first part of
/// the derived 32 bytes.
pub fn key_length(aes_type: &str) -> Result<usize> {
    match aes_type {
        "aes-128" => Ok(16),
        "aes-192" => Ok(24),
        "aes-256" => Ok(32),
        other => bail!("aes_type {other:?} is not one this crate knows"),
    }
}

/// Decrypt one block in place.
///
/// There is no padding. The stored length is already a multiple of the 16-byte block size
/// whenever encryption is on, so a length that is not is a corrupt file rather than a case
/// to handle.
pub fn decrypt_block(key: &[u8], iv: [u8; 16], buffer: &mut [u8]) -> Result<()> {
    ensure!(
        buffer.len().is_multiple_of(AES_BLOCK),
        "an encrypted block is {} bytes, which is not a multiple of {AES_BLOCK}",
        buffer.len()
    );

    let done = match key.len() {
        16 => cbc::Decryptor::<aes::Aes128>::new_from_slices(key, &iv)
            .map(|cipher| cipher.decrypt_padded::<NoPadding>(buffer).is_ok()),
        24 => cbc::Decryptor::<aes::Aes192>::new_from_slices(key, &iv)
            .map(|cipher| cipher.decrypt_padded::<NoPadding>(buffer).is_ok()),
        32 => cbc::Decryptor::<aes::Aes256>::new_from_slices(key, &iv)
            .map(|cipher| cipher.decrypt_padded::<NoPadding>(buffer).is_ok()),
        other => bail!("a {other}-byte key is not one AES takes"),
    };
    ensure!(
        done == Ok(true),
        "the key or the initialization vector is the wrong length for this block"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn key_derivation_matches_the_published_vector() {
        // RFC 6070 is defined for PBKDF2-HMAC-SHA1. This is the widely published
        // SHA-256 counterpart of its second case: password "password", salt "salt",
        // 2 iterations.
        let mut key = [0u8; 32];
        pbkdf2::pbkdf2_hmac::<Sha256>(b"password", b"salt", 2, &mut key);
        assert_eq!(
            hex(&key),
            "ae4d0c95af6b46d32d0adff928f06dd02a303f8ef3c251dfd6e2d85a95474c43"
        );
    }

    #[test]
    fn the_salt_is_the_hash_of_the_raw_image_id() {
        // The salt is SHA256 of the eight raw bytes, not of the sixteen text characters.
        let imageid = [0xDD, 0x5A, 0x77, 0xE6, 0xB6, 0x8A, 0x6C, 0x34];
        let key = derive_key("secret", imageid, 1000);

        let mut expected = [0u8; 32];
        pbkdf2::pbkdf2_hmac::<Sha256>(b"secret", &Sha256::digest(imageid), 1000, &mut expected);
        assert_eq!(key, expected);
        // And it differs from the mistake of hashing the text form.
        let mut wrong = [0u8; 32];
        pbkdf2::pbkdf2_hmac::<Sha256>(
            b"secret",
            &Sha256::digest(b"DD5A77E6B68A6C34"),
            1000,
            &mut wrong,
        );
        assert_ne!(key, wrong);
    }

    #[test]
    fn the_password_test_is_the_hmac_of_the_empty_message() {
        let key = [0x0bu8; 32];
        let mac = password_hmac(&key);

        // HMAC-SHA256 of the empty message under a key of 32 bytes of 0x0b.
        assert_eq!(hex(&mac).len(), 64);
        check_password(&key, &hex(&mac)).unwrap();
        check_password(&key, &hex(&mac).to_uppercase()).unwrap();

        let error = check_password(&key, &hex(&[0u8; 32])).unwrap_err();
        assert!(error.to_string().contains("the password is wrong"));
    }

    #[test]
    fn a_truncated_hmac_field_is_reported_rather_than_compared() {
        // The published ENCRYPTION.md shows a 63-character example, which is a typo.
        let key = [0u8; 32];
        let error = check_password(&key, &"a".repeat(63)).unwrap_err();
        assert!(error.to_string().contains("63 characters"), "{error}");
    }

    #[test]
    fn the_initialization_vector_uses_the_documented_tuple() {
        let key = [7u8; 32];
        let imageid = [1, 2, 3, 4, 5, 6, 7, 8];
        let iv = block_iv(&key, imageid, 0, 1, 2);

        // The same tuple encrypted by hand: AES-256-ECB under SHA256 of the key.
        let mut plain = [0u8; 16];
        plain[0..8].copy_from_slice(&imageid);
        plain[8..10].copy_from_slice(&0u16.to_le_bytes());
        plain[10..12].copy_from_slice(&1u16.to_le_bytes());
        plain[12..16].copy_from_slice(&2u32.to_le_bytes());
        let cipher = aes::Aes256::new(&Sha256::digest(key));
        let mut block = aes::cipher::Array(plain);
        cipher.encrypt_block(&mut block);
        assert_eq!(iv, block.0);

        // Every part of the tuple changes it.
        assert_ne!(iv, block_iv(&key, imageid, 1, 1, 2));
        assert_ne!(iv, block_iv(&key, imageid, 0, 2, 2));
        assert_ne!(iv, block_iv(&key, imageid, 0, 1, 3));
        assert_ne!(iv, block_iv(&key, [9; 8], 0, 1, 2));
    }

    #[test]
    fn decryption_undoes_the_reference_encryption() {
        // NIST SP 800-38A F.2.2, AES-128-CBC, the first two blocks of the vector.
        let key = [
            0x2b, 0x7e, 0x15, 0x16, 0x28, 0xae, 0xd2, 0xa6, 0xab, 0xf7, 0x15, 0x88, 0x09, 0xcf,
            0x4f, 0x3c,
        ];
        let iv = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f,
        ];
        let mut buffer = [
            0x76, 0x49, 0xab, 0xac, 0x81, 0x19, 0xb2, 0x46, 0xce, 0xe9, 0x8e, 0x9b, 0x12, 0xe9,
            0x19, 0x7d, 0x50, 0x86, 0xcb, 0x9b, 0x50, 0x72, 0x19, 0xee, 0x95, 0xdb, 0x11, 0x3a,
            0x91, 0x76, 0x78, 0xb2,
        ];

        decrypt_block(&key, iv, &mut buffer).unwrap();

        assert_eq!(
            hex(&buffer),
            "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51"
        );
    }

    #[test]
    fn a_length_that_is_not_a_whole_number_of_blocks_is_refused() {
        let mut buffer = [0u8; 17];
        let error = decrypt_block(&[0u8; 16], [0u8; 16], &mut buffer).unwrap_err();
        assert!(
            error.to_string().contains("not a multiple of 16"),
            "{error}"
        );
    }

    #[test]
    fn each_aes_type_takes_its_own_key_length() {
        assert_eq!(key_length("aes-128").unwrap(), 16);
        assert_eq!(key_length("aes-192").unwrap(), 24);
        assert_eq!(key_length("aes-256").unwrap(), 32);
        assert!(key_length("none").is_err());
    }
}
