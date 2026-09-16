//! The `$INDEX` block: the map from a partition's logical blocks to the bytes that hold
//! them, which may live in this file or in an earlier file of the same backup set.
//!
//! The two array elements are 30 and 34 bytes. Both are genuinely unaligned, so they are
//! read and written by byte offset. A language that pads `DataBlockIndexElement` out to 32
//! bytes produces a file that no Macrium tool can read, and it does so silently. That is
//! the single most likely corruption bug in this crate, which is why the sizes are
//! asserted in the tests below.

use anyhow::{ensure, Context, Result};

/// Serialized size of [`DataBlockIndexElement`]: `i64` + `[u8; 16]` + `u32` + `u16`.
pub const ELEMENT_LEN: usize = 30;

/// Serialized size of [`DeltaDataBlock`]: an element plus a trailing `u32`.
pub const DELTA_LEN: usize = 34;

/// One logical block of a partition.
///
/// `md5_hash` covers the **plaintext**, that is after decryption and decompression. This
/// is the opposite of the metadata block hash, which covers the stored bytes.
///
/// `block_length` is the **stored** length. It already accounts for compression and for
/// the 16-byte rounding that in-place AES needs, which is what makes a byte-for-byte copy
/// between files legal.
///
/// A `block_length` of zero means the block was never captured. It is a hole, not an
/// error, and a restore writes nothing for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DataBlockIndexElement {
    pub file_position: i64,
    pub md5_hash: [u8; 16],
    pub block_length: u32,
    /// Which file of the backup set holds these bytes.
    pub file_number: u16,
}

impl DataBlockIndexElement {
    pub fn parse(raw: &[u8]) -> Result<Self> {
        ensure!(
            raw.len() >= ELEMENT_LEN,
            "index element needs {ELEMENT_LEN} bytes, got {}",
            raw.len()
        );
        let mut md5_hash = [0u8; 16];
        md5_hash.copy_from_slice(&raw[8..24]);
        Ok(Self {
            file_position: i64::from_le_bytes(raw[0..8].try_into().expect("8 bytes")),
            md5_hash,
            block_length: u32::from_le_bytes(raw[24..28].try_into().expect("4 bytes")),
            file_number: u16::from_le_bytes(raw[28..30].try_into().expect("2 bytes")),
        })
    }

    pub fn write_to(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.file_position.to_le_bytes());
        out.extend_from_slice(&self.md5_hash);
        out.extend_from_slice(&self.block_length.to_le_bytes());
        out.extend_from_slice(&self.file_number.to_le_bytes());
    }

    /// True when the block was never captured.
    pub fn is_hole(&self) -> bool {
        self.block_length == 0
    }
}

/// One changed block in a delta index, together with the logical position it replaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DeltaDataBlock {
    pub element: DataBlockIndexElement,
    /// The position in the flattened array that this block overwrites.
    pub block_index: u32,
}

impl DeltaDataBlock {
    pub fn parse(raw: &[u8]) -> Result<Self> {
        ensure!(
            raw.len() >= DELTA_LEN,
            "delta index element needs {DELTA_LEN} bytes, got {}",
            raw.len()
        );
        Ok(Self {
            element: DataBlockIndexElement::parse(raw)?,
            block_index: u32::from_le_bytes(raw[30..34].try_into().expect("4 bytes")),
        })
    }

    pub fn write_to(&self, out: &mut Vec<u8>) {
        self.element.write_to(out);
        out.extend_from_slice(&self.block_index.to_le_bytes());
    }
}

/// The block array of one partition, in whichever of the two forms the file uses.
///
/// Which form appears is decided by the JSON field `_header.delta_index` alone. Do not
/// infer it from `backup_type`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocks {
    /// A dense array covering every logical block of the partition.
    Full(Vec<DataBlockIndexElement>),
    /// Only the blocks that changed, each tagged with the position it replaces.
    Delta(Vec<DeltaDataBlock>),
}

impl Blocks {
    pub fn len(&self) -> usize {
        match self {
            Blocks::Full(v) => v.len(),
            Blocks::Delta(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn is_delta(&self) -> bool {
        matches!(self, Blocks::Delta(_))
    }
}

/// A parsed `$INDEX` payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartitionIndex {
    /// FAT reserved sectors, segmented into blocks. Never flattened across the chain: the
    /// reference restore takes this array wholesale from the newest file.
    pub reserved: Vec<DataBlockIndexElement>,
    pub blocks: Blocks,
}

impl PartitionIndex {
    /// Parse a `$INDEX` payload.
    ///
    /// `delta_index` comes from the file's JSON header and decides the second array's
    /// element type. The whole payload must be consumed exactly; a leftover tail means the
    /// caller passed the wrong `delta_index` and the arrays were read at the wrong stride.
    pub fn parse(raw: &[u8], delta_index: bool) -> Result<Self> {
        let mut at = 0usize;
        let reserved_count = read_count(raw, &mut at, "reserved sectors")?;
        let reserved = parse_array(raw, &mut at, reserved_count, ELEMENT_LEN, |b| {
            DataBlockIndexElement::parse(b)
        })
        .context("parsing the reserved sectors index")?;

        let block_count = read_count(raw, &mut at, "data blocks")?;
        let blocks = if delta_index {
            Blocks::Delta(
                parse_array(raw, &mut at, block_count, DELTA_LEN, DeltaDataBlock::parse)
                    .context("parsing the delta block index")?,
            )
        } else {
            Blocks::Full(
                parse_array(raw, &mut at, block_count, ELEMENT_LEN, |b| {
                    DataBlockIndexElement::parse(b)
                })
                .context("parsing the data block index")?,
            )
        };

        ensure!(
            at == raw.len(),
            "$INDEX payload has {} trailing bytes after {at} of {}; \
             delta_index={delta_index} is probably wrong for this file",
            raw.len() - at,
            raw.len()
        );
        Ok(Self { reserved, blocks })
    }

    /// Serialize back to a `$INDEX` payload.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(
            8 + self.reserved.len() * ELEMENT_LEN
                + self.blocks.len()
                    * if self.blocks.is_delta() {
                        DELTA_LEN
                    } else {
                        ELEMENT_LEN
                    },
        );
        write_count(&mut out, self.reserved.len());
        for e in &self.reserved {
            e.write_to(&mut out);
        }
        write_count(&mut out, self.blocks.len());
        match &self.blocks {
            Blocks::Full(v) => {
                for e in v {
                    e.write_to(&mut out);
                }
            }
            Blocks::Delta(v) => {
                for d in v {
                    d.write_to(&mut out);
                }
            }
        }
        out
    }
}

/// The counts are written as a signed 32-bit integer. The reference reader treats a value
/// at or below zero as "no array", so we do the same rather than rejecting it.
fn read_count(raw: &[u8], at: &mut usize, what: &str) -> Result<usize> {
    ensure!(
        raw.len() >= *at + 4,
        "$INDEX payload ends before the {what} count"
    );
    let n = i32::from_le_bytes(raw[*at..*at + 4].try_into().expect("4 bytes"));
    *at += 4;
    Ok(n.max(0) as usize)
}

fn write_count(out: &mut Vec<u8>, count: usize) {
    let n = i32::try_from(count).unwrap_or(i32::MAX);
    out.extend_from_slice(&n.to_le_bytes());
}

fn parse_array<T>(
    raw: &[u8],
    at: &mut usize,
    count: usize,
    stride: usize,
    parse: impl Fn(&[u8]) -> Result<T>,
) -> Result<Vec<T>> {
    let needed = count
        .checked_mul(stride)
        .context("index array length overflows")?;
    ensure!(
        raw.len() >= *at + needed,
        "$INDEX payload holds {} bytes but the array needs {needed} from offset {at}",
        raw.len()
    );
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let start = *at + i * stride;
        out.push(parse(&raw[start..start + stride])?);
    }
    *at += needed;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn element_sizes_are_exact() {
        // If either of these ever changes, every index in every file becomes garbage.
        assert_eq!(ELEMENT_LEN, 30);
        assert_eq!(DELTA_LEN, 34);
        let mut buf = Vec::new();
        DataBlockIndexElement::default().write_to(&mut buf);
        assert_eq!(buf.len(), ELEMENT_LEN);
        buf.clear();
        DeltaDataBlock::default().write_to(&mut buf);
        assert_eq!(buf.len(), DELTA_LEN);
    }

    #[test]
    fn element_fields_sit_at_the_documented_offsets() {
        let e = DataBlockIndexElement {
            file_position: 0x0102_0304_0506_0708,
            md5_hash: [0xAA; 16],
            block_length: 0x1112_1314,
            file_number: 0x2122,
        };
        let mut buf = Vec::new();
        e.write_to(&mut buf);
        assert_eq!(&buf[0..8], &0x0102_0304_0506_0708i64.to_le_bytes());
        assert_eq!(&buf[8..24], &[0xAA; 16]);
        assert_eq!(&buf[24..28], &0x1112_1314u32.to_le_bytes());
        assert_eq!(&buf[28..30], &0x2122u16.to_le_bytes());
        assert_eq!(DataBlockIndexElement::parse(&buf).unwrap(), e);
    }

    #[test]
    fn delta_carries_the_block_index_after_the_element() {
        let d = DeltaDataBlock {
            element: DataBlockIndexElement {
                file_position: 42,
                md5_hash: [1; 16],
                block_length: 64,
                file_number: 3,
            },
            block_index: 0x0A0B_0C0D,
        };
        let mut buf = Vec::new();
        d.write_to(&mut buf);
        assert_eq!(&buf[30..34], &0x0A0B_0C0Du32.to_le_bytes());
        assert_eq!(DeltaDataBlock::parse(&buf).unwrap(), d);
    }

    fn sample(full: bool) -> PartitionIndex {
        let e = |n: u16| DataBlockIndexElement {
            file_position: i64::from(n) * 1000,
            md5_hash: [n as u8; 16],
            block_length: 64,
            file_number: n,
        };
        PartitionIndex {
            reserved: vec![e(0), e(1)],
            blocks: if full {
                Blocks::Full(vec![e(2), DataBlockIndexElement::default(), e(3)])
            } else {
                Blocks::Delta(vec![DeltaDataBlock {
                    element: e(4),
                    block_index: 9,
                }])
            },
        }
    }

    #[test]
    fn full_index_round_trips() {
        let idx = sample(true);
        let bytes = idx.to_bytes();
        assert_eq!(bytes.len(), 4 + 2 * 30 + 4 + 3 * 30);
        assert_eq!(PartitionIndex::parse(&bytes, false).unwrap(), idx);
    }

    #[test]
    fn delta_index_round_trips() {
        let idx = sample(false);
        let bytes = idx.to_bytes();
        assert_eq!(bytes.len(), 4 + 2 * 30 + 4 + 34);
        assert_eq!(PartitionIndex::parse(&bytes, true).unwrap(), idx);
    }

    #[test]
    fn the_wrong_delta_flag_is_caught_rather_than_silently_misparsed() {
        // Reading a 34-byte array at a 30-byte stride leaves a tail. Without the
        // exact-consumption check this would parse into plausible nonsense.
        let bytes = sample(false).to_bytes();
        let err = PartitionIndex::parse(&bytes, false).unwrap_err();
        assert!(err.to_string().contains("trailing bytes"), "{err}");
    }

    #[test]
    fn a_hole_survives_the_round_trip_as_a_hole() {
        let idx = sample(true);
        let bytes = idx.to_bytes();
        let back = PartitionIndex::parse(&bytes, false).unwrap();
        let Blocks::Full(v) = &back.blocks else {
            panic!("expected a full index");
        };
        assert!(v[1].is_hole());
        assert_eq!(v[1], DataBlockIndexElement::default());
    }

    #[test]
    fn a_truncated_array_is_rejected() {
        let mut bytes = sample(true).to_bytes();
        bytes.truncate(bytes.len() - 5);
        assert!(PartitionIndex::parse(&bytes, false).is_err());
    }
}
