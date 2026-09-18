//! The optional end-to-end test: decrypt, decompress and hash every block of a file.
//!
//! A merge never needs this. It copies stored bytes, so the plaintext hash it carries stays
//! correct by construction. This module exists to prove that claim on real data rather than
//! to assert it: it takes the output, undoes everything the format did to each block, and
//! compares the result against the hash the index records.
//!
//! The reference restore only tests `md5_hash` when compression is on, so an uncompressed
//! set gets no integrity test from it at all. This tests every block either way.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{bail, ensure, Context, Result};
use serde_json::Value;

use crate::block::md5;
use crate::crypto;
use crate::index::{Blocks, DataBlockIndexElement};
use crate::reader::BackupFile;

/// What a verify pass found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Verified {
    /// Blocks that were read, undone and hashed.
    pub blocks: usize,
    /// Plaintext bytes hashed.
    pub bytes: u64,
    /// Blocks that live in another file of the set, which this pass does not open.
    pub elsewhere: usize,
}

/// The two forms of the key one file needs.
struct Key {
    /// Cut to the length `aes_type` asks for, which is what the payload cipher takes.
    cipher: Vec<u8>,
    /// The whole 32 bytes, which is what the initialization vector always uses.
    derived: [u8; crypto::KEY_LEN],
}

/// Which disk and partition a block belongs to, as the document numbers them.
#[derive(Debug, Clone, Copy)]
struct Where {
    disk_number: u16,
    partition_number: u16,
}

/// Read the numbers the document records for one disk and partition.
///
/// These are not the array positions. The encrypted test set records its only disk as number
/// 2, and a block of it decrypts only under 2.
fn numbering(document: &Value, disk: usize, partition: usize) -> Result<Where> {
    let disk_value = document
        .get("disks")
        .and_then(|d| d.get(disk))
        .with_context(|| format!("the document has no disks[{disk}]"))?;
    let number = |value: &Value, object: &str, key: &str| -> Result<u16> {
        let found = value
            .get(object)
            .and_then(|o| o.get(key))
            .and_then(Value::as_u64)
            .with_context(|| format!("{object}.{key} is missing"))?;
        Ok(u16::try_from(found)?)
    };

    let partition_value = disk_value
        .get("partitions")
        .and_then(|p| p.get(partition))
        .with_context(|| format!("the document has no disks[{disk}].partitions[{partition}]"))?;

    Ok(Where {
        disk_number: number(disk_value, "_header", "disk_number")?,
        partition_number: number(partition_value, "_header", "partition_number")?,
    })
}

/// How each block of one file is stored.
struct Storage {
    compressed: bool,
    /// Absent when the set is not encrypted.
    key: Option<Key>,
    imageid: [u8; 8],
}

impl Storage {
    /// Read the settings, and derive and test the password when there is one.
    fn of(file: &BackupFile, password: Option<&str>) -> Result<Self> {
        let setting = |object: &str, key: &str| -> Option<Value> {
            file.json.get(object).and_then(|o| o.get(key)).cloned()
        };
        let compressed = setting("_compression", "compression_level")
            .and_then(|v| v.as_str().map(|s| s != "none"))
            .unwrap_or(false);
        let encrypted = setting("_encryption", "enable")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let imageid = file.header.imageid_binary()?;

        if !encrypted {
            return Ok(Self {
                compressed,
                key: None,
                imageid,
            });
        }

        let password = password.context(
            "this set is encrypted, so the end-to-end test needs its password. \
             Put it in MRIMGX_PASSWORD",
        )?;
        let aes_type = setting("_encryption", "aes_type")
            .and_then(|v| v.as_str().map(str::to_string))
            .context("_encryption.aes_type is missing")?;
        let iterations = setting("_encryption", "key_iterations")
            .and_then(|v| v.as_u64())
            .context("_encryption.key_iterations is missing")?;
        let stored_hmac = setting("_encryption", "hmac")
            .and_then(|v| v.as_str().map(str::to_string))
            .context("_encryption.hmac is missing")?;

        let derived = crypto::derive_key(password, imageid, u32::try_from(iterations)?);
        crypto::check_password(&derived, &stored_hmac)?;
        let length = crypto::key_length(&aes_type)?;

        Ok(Self {
            compressed,
            key: Some(Key {
                cipher: derived[..length].to_vec(),
                derived,
            }),
            imageid,
        })
    }

    /// Undo whatever was done to one block, and return its plaintext.
    ///
    /// `at` carries the numbers the document records, not the positions of the arrays they
    /// sit in. Measured on the encrypted test set, where the disk is recorded as number 2
    /// and sits at array position 0: only the recorded number decrypts.
    fn plaintext(&self, mut stored: Vec<u8>, at: Where, block_index: u32) -> Result<Vec<u8>> {
        if let Some(key) = &self.key {
            let iv = crypto::block_iv(
                &key.derived,
                self.imageid,
                at.disk_number,
                at.partition_number,
                block_index,
            );
            crypto::decrypt_block(&key.cipher, iv, &mut stored)?;
        }
        if !self.compressed {
            return Ok(stored);
        }
        // One frame, and only one. Encryption pads the stored length up to a multiple of
        // sixteen, so there can be bytes after the frame that are not a frame.
        let mut plain = Vec::new();
        zstd::stream::read::Decoder::new(stored.as_slice())
            .context("starting the decompressor")?
            .single_frame()
            .read_to_end(&mut plain)
            .context("decompressing a block")?;
        Ok(plain)
    }
}

/// Read every block of `path` that the file itself holds, undo it, and hash it.
///
/// A block owned by another file of the set is counted and skipped, because this pass opens
/// one file. `password` is needed only for an encrypted set.
pub fn verify_file(path: &Path, password: Option<&str>) -> Result<Verified> {
    let file = BackupFile::open(path, true)?;
    let storage = Storage::of(&file, password)?;
    let own = file.header.file_number;

    let handle = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut reader = BufReader::new(handle);
    let mut report = Verified::default();

    for (d, disk) in file.disks.iter().enumerate() {
        for (p, part) in disk.partitions.iter().enumerate() {
            let at = numbering(&file.json, d, p)?;
            let mut check = |entry: &DataBlockIndexElement, block_index: u32| -> Result<()> {
                if entry.is_hole() {
                    return Ok(());
                }
                if entry.file_number != own {
                    report.elsewhere += 1;
                    return Ok(());
                }

                let mut stored = vec![0u8; entry.block_length as usize];
                reader.seek(SeekFrom::Start(entry.file_position.max(0) as u64))?;
                reader.read_exact(&mut stored).with_context(|| {
                    format!(
                        "reading {} bytes at {} of disk {d} partition {p}",
                        entry.block_length, entry.file_position
                    )
                })?;

                let plain = storage
                    .plaintext(stored, at, block_index)
                    .with_context(|| {
                        format!("undoing block {block_index} of disk {d} partition {p}")
                    })?;
                ensure!(
                    md5(&plain) == entry.md5_hash,
                    "block {block_index} of disk {d} partition {p} does not match the hash \
                     the index records for it"
                );
                report.blocks += 1;
                report.bytes += plain.len() as u64;
                Ok(())
            };

            // The reserved sectors count from zero in their own array, so a reserved block
            // and a data block at the same position share an initialization vector. The
            // reference restore does exactly this.
            for (i, entry) in part.index.reserved.iter().enumerate() {
                check(entry, u32::try_from(i)?)?;
            }
            match &part.index.blocks {
                Blocks::Full(entries) => {
                    for (i, entry) in entries.iter().enumerate() {
                        check(entry, u32::try_from(i)?)?;
                    }
                }
                Blocks::Delta(entries) => {
                    for delta in entries {
                        check(&delta.element, delta.block_index)?;
                    }
                }
            }
        }
    }

    if report.blocks == 0 {
        bail!("{} holds no block of its own to test", path.display());
    }
    Ok(report)
}
