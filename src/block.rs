//! The on-disk framing of a Macrium Reflect X backup file: the footer, the metadata
//! block header, and the linked lists those headers form.
//!
//! A backup file has no header at offset 0. Data blocks come first, metadata follows, and
//! the last 20 bytes are the footer. Everything is little-endian. Structure sizes are
//! exact and unaligned, so every field is read and written by offset rather than by
//! casting a packed struct. See `scratch/format-notes.md` for the derivation.

use std::io::{Read, Seek, SeekFrom};

use anyhow::{bail, ensure, Context, Result};

/// Trailing magic bytes, ASCII, with no terminator.
pub const MAGIC: &[u8; 12] = b"MACRIUM_FILE";

/// `u64 first_metadata_block_header` followed by [`MAGIC`].
pub const FOOTER_LEN: u64 = 8 + MAGIC.len() as u64;

/// Serialized size of [`MetadataBlockHeader`].
pub const HEADER_LEN: usize = 32;

/// Block names are eight ASCII bytes padded with spaces.
pub const NAME_LEN: usize = 8;

pub const JSON: &[u8; NAME_LEN] = b"$JSON   ";
pub const BITMAP: &[u8; NAME_LEN] = b"$BITMAP ";
pub const FAT: &[u8; NAME_LEN] = b"$FAT    ";
pub const CBT: &[u8; NAME_LEN] = b"$CBT    ";
pub const MFT: &[u8; NAME_LEN] = b"$MFT    ";
pub const TRACK0: &[u8; NAME_LEN] = b"$TRACK0 ";
pub const INDEX: &[u8; NAME_LEN] = b"$INDEX  ";
pub const EPT: &[u8; NAME_LEN] = b"$EPT    ";
pub const AUXDATA: &[u8; NAME_LEN] = b"$AUXDATA";

/// A guard against walking a corrupt list forever. Real files hold at most a handful of
/// blocks per list.
const MAX_BLOCKS_PER_LIST: usize = 64;

/// The data region is padded up to a multiple of this before the metadata region starts,
/// so `_header.index_file_position` is always a multiple of it.
///
/// Measured, not documented. It holds on all sixteen files of both test sets, which
/// between them cover uncompressed and high-compression, MBR and GPT, FAT and NTFS, and
/// sizes from 3 MB to 3.7 GB. The uncompressed corpus hides the padding, because
/// uncompressed blocks land on the boundary anyway and the gap is zero. A compressed set
/// shows gaps of 1222, 2376 and 3816 bytes.
pub const DATA_ALIGNMENT: u64 = 4096;

/// Round `offset` up to the next [`DATA_ALIGNMENT`] boundary.
pub fn align_up(offset: u64) -> u64 {
    offset.div_ceil(DATA_ALIGNMENT) * DATA_ALIGNMENT
}

/// The three flag bits packed into byte 28 of a block header.
///
/// The remaining five bits are unused and are written as zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Flags {
    /// This is the final block of its list.
    pub last_block: bool,
    /// The payload is a zstd frame.
    pub compression: bool,
    /// The payload is AES encrypted.
    pub encryption: bool,
}

impl Flags {
    const LAST_BLOCK: u8 = 0x01;
    const COMPRESSION: u8 = 0x02;
    const ENCRYPTION: u8 = 0x04;

    pub fn from_byte(byte: u8) -> Self {
        Self {
            last_block: byte & Self::LAST_BLOCK != 0,
            compression: byte & Self::COMPRESSION != 0,
            encryption: byte & Self::ENCRYPTION != 0,
        }
    }

    pub fn to_byte(self) -> u8 {
        let mut byte = 0;
        if self.last_block {
            byte |= Self::LAST_BLOCK;
        }
        if self.compression {
            byte |= Self::COMPRESSION;
        }
        if self.encryption {
            byte |= Self::ENCRYPTION;
        }
        byte
    }

    /// The flags a freshly written plaintext block carries.
    pub fn plain(last_block: bool) -> Self {
        Self {
            last_block,
            compression: false,
            encryption: false,
        }
    }
}

/// A 32-byte metadata block header.
///
/// `hash` covers the **stored** bytes, that is after compression and after encryption.
/// This differs from the data block hash, which covers the plaintext. Mixing the two up
/// is the classic way to produce a file that the reference reader rejects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataBlockHeader {
    pub name: [u8; NAME_LEN],
    pub block_length: u32,
    pub hash: [u8; 16],
    pub flags: Flags,
}

impl MetadataBlockHeader {
    pub fn parse(raw: &[u8]) -> Result<Self> {
        ensure!(
            raw.len() >= HEADER_LEN,
            "metadata block header needs {HEADER_LEN} bytes, got {}",
            raw.len()
        );
        let mut name = [0u8; NAME_LEN];
        name.copy_from_slice(&raw[0..8]);
        let mut hash = [0u8; 16];
        hash.copy_from_slice(&raw[12..28]);
        Ok(Self {
            name,
            block_length: u32::from_le_bytes(raw[8..12].try_into().expect("4 bytes")),
            hash,
            flags: Flags::from_byte(raw[28]),
        })
    }

    pub fn to_bytes(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[0..8].copy_from_slice(&self.name);
        out[8..12].copy_from_slice(&self.block_length.to_le_bytes());
        out[12..28].copy_from_slice(&self.hash);
        out[28] = self.flags.to_byte();
        // Bytes 29 to 31 are explicit padding and stay zero.
        out
    }

    /// Build a header for a plaintext payload, hashing it as the format requires.
    pub fn for_payload(name: &[u8; NAME_LEN], payload: &[u8], last_block: bool) -> Result<Self> {
        let block_length = u32::try_from(payload.len())
            .with_context(|| format!("{} payload is larger than 4 GiB", name_str(name)))?;
        Ok(Self {
            name: *name,
            block_length,
            hash: md5(payload),
            flags: Flags::plain(last_block),
        })
    }

    pub fn is(&self, name: &[u8; NAME_LEN]) -> bool {
        &self.name == name
    }

    pub fn name_str(&self) -> String {
        name_str(&self.name)
    }
}

/// One block of a metadata list, located within the file.
#[derive(Debug, Clone)]
pub struct Located {
    pub header: MetadataBlockHeader,
    /// Offset of the 32-byte header itself.
    pub offset: u64,
}

impl Located {
    /// Offset of the first payload byte.
    pub fn payload_at(&self) -> u64 {
        self.offset + HEADER_LEN as u64
    }

    /// Offset one past the last payload byte, which is where the next block starts.
    pub fn end(&self) -> u64 {
        self.payload_at() + u64::from(self.header.block_length)
    }
}

/// One metadata list: the blocks it holds, and where it ends.
#[derive(Debug, Clone)]
pub struct BlockList {
    pub blocks: Vec<Located>,
    /// Offset one past the final block, which is where the next list starts.
    pub end: u64,
}

impl BlockList {
    pub fn find(&self, name: &[u8; NAME_LEN]) -> Option<&Located> {
        self.blocks.iter().find(|b| b.header.is(name))
    }

    /// The final block of the list. A list always holds at least one block.
    pub fn last(&self) -> &Located {
        self.blocks.last().expect("a block list is never empty")
    }
}

/// Read the footer and return the offset of the root metadata list.
///
/// This seeks to the end of the file first, so the caller does not need to know its size.
pub fn read_footer<R: Read + Seek>(reader: &mut R) -> Result<u64> {
    let size = reader.seek(SeekFrom::End(0))?;
    ensure!(
        size >= FOOTER_LEN,
        "file is {size} bytes, too short to hold a {FOOTER_LEN}-byte footer"
    );
    reader.seek(SeekFrom::End(-(FOOTER_LEN as i64)))?;
    let mut raw = [0u8; FOOTER_LEN as usize];
    reader.read_exact(&mut raw).context("reading the footer")?;
    if &raw[8..] != MAGIC {
        bail!("Invalid file: not a Macrium Reflect vX file.");
    }
    Ok(u64::from_le_bytes(raw[0..8].try_into().expect("8 bytes")))
}

/// Serialize a footer pointing at `root_list_offset`.
pub fn footer_bytes(root_list_offset: u64) -> [u8; FOOTER_LEN as usize] {
    let mut out = [0u8; FOOTER_LEN as usize];
    out[0..8].copy_from_slice(&root_list_offset.to_le_bytes());
    out[8..].copy_from_slice(MAGIC);
    out
}

/// Walk one singly-linked metadata list starting at `start`.
///
/// A list ends at the block whose `last_block` flag is set. Unknown block names are
/// skipped by length rather than rejected, because `FILE_LAYOUT.md` states that not every
/// block name is documented.
pub fn walk_list<R: Read + Seek>(reader: &mut R, start: u64) -> Result<BlockList> {
    let mut blocks = Vec::new();
    let mut offset = start;
    loop {
        reader.seek(SeekFrom::Start(offset))?;
        let mut raw = [0u8; HEADER_LEN];
        reader
            .read_exact(&mut raw)
            .with_context(|| format!("reading a metadata block header at offset {offset}"))?;
        let header = MetadataBlockHeader::parse(&raw)?;
        let last = header.flags.last_block;
        let located = Located { header, offset };
        offset = located.end();
        blocks.push(located);
        if last {
            return Ok(BlockList {
                blocks,
                end: offset,
            });
        }
        ensure!(
            blocks.len() < MAX_BLOCKS_PER_LIST,
            "metadata list starting at {start} has more than {MAX_BLOCKS_PER_LIST} blocks; \
             the file is corrupt or this is not a metadata list"
        );
    }
}

/// Read a block's payload and undo whatever its flags say was done to it.
///
/// The order matters and follows the reference `readBlock`: hash first, then decrypt, then
/// decompress. `hash` covers the stored bytes, so it is checked before anything is undone.
/// That way a corrupt block is reported as corrupt rather than as a decompression failure.
///
/// Encrypted metadata blocks are not supported yet and are reported rather than guessed at.
pub fn read_block<R: Read + Seek>(reader: &mut R, block: &Located) -> Result<Vec<u8>> {
    let stored = read_payload(reader, block)?;
    let name = block.header.name_str();

    if md5(&stored) != block.header.hash {
        bail!("Block hash mismatch. ({name} at offset {})", block.offset);
    }

    ensure!(
        !block.header.flags.encryption,
        "the {name} block is encrypted; this build cannot decrypt metadata yet"
    );

    if block.header.flags.compression {
        // The frame header carries the content size, so the decoder needs no hint.
        return zstd::decode_all(stored.as_slice())
            .with_context(|| format!("Failed to decompress block. ({name})"));
    }
    Ok(stored)
}

/// Read a block's payload exactly as stored, with no decompression and no decryption.
///
/// This is what a verbatim copy uses. Nothing is checked, because the stored bytes are the
/// thing being preserved.
pub fn read_payload<R: Read + Seek>(reader: &mut R, block: &Located) -> Result<Vec<u8>> {
    reader.seek(SeekFrom::Start(block.payload_at()))?;
    let mut buf = vec![0u8; block.header.block_length as usize];
    reader.read_exact(&mut buf).with_context(|| {
        format!(
            "reading the {} payload at offset {}",
            block.header.name_str(),
            block.payload_at()
        )
    })?;
    Ok(buf)
}

pub fn md5(bytes: &[u8]) -> [u8; 16] {
    use md5::{Digest, Md5};
    let mut hasher = Md5::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

fn name_str(name: &[u8; NAME_LEN]) -> String {
    String::from_utf8_lossy(name).trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn footer_is_twenty_bytes() {
        assert_eq!(FOOTER_LEN, 20);
        assert_eq!(MAGIC.len(), 12);
        // The magic has no NUL terminator. A C literal would have carried one.
        assert!(!MAGIC.contains(&0));
    }

    #[test]
    fn header_is_thirty_two_bytes() {
        assert_eq!(HEADER_LEN, 32);
        let hdr = MetadataBlockHeader::for_payload(JSON, b"{}", false).unwrap();
        assert_eq!(hdr.to_bytes().len(), 32);
    }

    #[test]
    fn flag_bits_match_the_msvc_bitfield_order() {
        assert_eq!(
            Flags::from_byte(0x01),
            Flags {
                last_block: true,
                compression: false,
                encryption: false
            }
        );
        assert_eq!(
            Flags::from_byte(0x02),
            Flags {
                last_block: false,
                compression: true,
                encryption: false
            }
        );
        assert_eq!(
            Flags::from_byte(0x04),
            Flags {
                last_block: false,
                compression: false,
                encryption: true
            }
        );
        // The top five bits are unused and must not leak into the parse.
        assert_eq!(Flags::from_byte(0xF8), Flags::default());
    }

    #[test]
    fn header_round_trips() {
        let hdr = MetadataBlockHeader {
            name: *INDEX,
            block_length: 0x1234_5678,
            hash: [7u8; 16],
            flags: Flags {
                last_block: true,
                compression: false,
                encryption: true,
            },
        };
        let bytes = hdr.to_bytes();
        assert_eq!(MetadataBlockHeader::parse(&bytes).unwrap(), hdr);
        // Padding stays zero.
        assert_eq!(&bytes[29..32], &[0, 0, 0]);
    }

    #[test]
    fn footer_round_trips() {
        let mut file = footer_bytes(0x00AB_CDEF).to_vec();
        let mut cursor = Cursor::new(&mut file);
        assert_eq!(read_footer(&mut cursor).unwrap(), 0x00AB_CDEF);
    }

    #[test]
    fn a_bad_magic_is_rejected_with_the_reference_message() {
        let mut file = vec![0u8; 20];
        let err = read_footer(&mut Cursor::new(&mut file)).unwrap_err();
        assert_eq!(
            err.to_string(),
            "Invalid file: not a Macrium Reflect vX file."
        );
    }

    /// Build a two-block list so the walk has to follow `last_block` rather than stop at
    /// the first header.
    fn two_block_list() -> Vec<u8> {
        let mut out = Vec::new();
        let first = MetadataBlockHeader::for_payload(BITMAP, b"bitmap payload", false).unwrap();
        out.extend_from_slice(&first.to_bytes());
        out.extend_from_slice(b"bitmap payload");
        let second = MetadataBlockHeader::for_payload(INDEX, b"index", true).unwrap();
        out.extend_from_slice(&second.to_bytes());
        out.extend_from_slice(b"index");
        out
    }

    #[test]
    fn walk_follows_the_chain_and_reports_the_end() {
        let mut bytes = two_block_list();
        let total = bytes.len() as u64;
        let list = walk_list(&mut Cursor::new(&mut bytes), 0).unwrap();
        assert_eq!(list.blocks.len(), 2);
        assert!(list.blocks[0].header.is(BITMAP));
        assert!(list.last().header.is(INDEX));
        assert_eq!(list.end, total);
    }

    #[test]
    fn payloads_read_back_verbatim() {
        let mut bytes = two_block_list();
        let list = walk_list(&mut Cursor::new(&mut bytes), 0).unwrap();
        let mut cursor = Cursor::new(&mut bytes);
        assert_eq!(
            read_payload(&mut cursor, &list.blocks[0]).unwrap(),
            b"bitmap payload"
        );
        assert_eq!(read_payload(&mut cursor, list.last()).unwrap(), b"index");
    }

    #[test]
    fn a_list_with_no_terminator_does_not_loop_forever() {
        // Every header says "not the last block", so the walk must give up.
        let hdr = MetadataBlockHeader::for_payload(BITMAP, b"", false).unwrap();
        let mut bytes = hdr.to_bytes().repeat(MAX_BLOCKS_PER_LIST + 4);
        let err = walk_list(&mut Cursor::new(&mut bytes), 0).unwrap_err();
        assert!(err.to_string().contains("more than"), "{err}");
    }
}
