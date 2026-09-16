//! Writing the data region of a consolidated file.
//!
//! This is the forward, append-only pass. Blocks are copied byte for byte: nothing is
//! decompressed, nothing is re-encrypted, and `block_length` and `md5_hash` are carried
//! through untouched. Only `file_position` and `file_number` change, because only those two
//! describe where the bytes live rather than what they are.
//!
//! That is sound because the per-block encryption initialization vector is derived from the
//! image id, the disk number, the partition number and the logical block index. A merge
//! preserves all four. See `scratch/format-notes.md`.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use anyhow::{ensure, Context, Result};

use crate::block::align_up;
use crate::index::{Blocks, DataBlockIndexElement, DeltaDataBlock, PartitionIndex};
use crate::plan::{Action, MergePlan, PartitionPlan};
use crate::set::BackupSet;

/// Somewhere to read source blocks from.
///
/// Abstracted so the copy loop can be tested without a backup set on disk. There are two
/// implementations: [`SourceFiles`] over real files, and a fake one in the tests.
pub trait BlockSource {
    /// Read exactly `length` bytes at `position` from the file that `file_number` names.
    fn read_block(&mut self, file_number: u16, position: i64, length: u32) -> Result<Vec<u8>>;
}

/// The open source files of a merge, keyed by every file number they answer for.
pub struct SourceFiles {
    handles: HashMap<u16, BufReader<File>>,
}

impl SourceFiles {
    /// Open every file the plan reads from, read-only.
    ///
    /// A member answers for its own file number and for every number it absorbed, so two
    /// keys can name one file. Each path is opened once.
    pub fn open(set: &BackupSet, plan: &MergePlan) -> Result<Self> {
        let mut handles = HashMap::new();
        for number in plan.absorbed.iter().copied() {
            let owner = set
                .owner(number)
                .with_context(|| format!("no member of the set owns file number {number}"))?;
            let path: &PathBuf = &owner.path;
            // One reader per file number, even when two numbers name the same file. Each
            // keeps its own cursor, which keeps the copy loop free of seek bookkeeping.
            let file = File::open(path)
                .with_context(|| format!("opening {} to read blocks", path.display()))?;
            handles.insert(number, BufReader::new(file));
        }
        Ok(Self { handles })
    }
}

impl BlockSource for SourceFiles {
    fn read_block(&mut self, file_number: u16, position: i64, length: u32) -> Result<Vec<u8>> {
        let reader = self
            .handles
            .get_mut(&file_number)
            .with_context(|| format!("no open handle for file number {file_number}"))?;
        ensure!(position >= 0, "block position {position} is negative");
        reader
            .seek(SeekFrom::Start(position as u64))
            .with_context(|| format!("seeking to {position} in file {file_number}"))?;
        let mut buf = vec![0u8; length as usize];
        reader.read_exact(&mut buf).with_context(|| {
            format!("reading {length} bytes at {position} from file {file_number}")
        })?;
        Ok(buf)
    }
}

/// One partition's index, after the data region has been written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrittenPartition {
    pub disk: usize,
    pub partition: usize,
    pub index: PartitionIndex,
}

/// The result of writing the data region.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataRegion {
    pub partitions: Vec<WrittenPartition>,
    /// Bytes of block payload written, before padding.
    pub payload_bytes: u64,
    /// Padded end of the region. This becomes `_header.index_file_position`.
    pub end: u64,
    /// How many copies the duplicate map avoided.
    pub duplicates_avoided: usize,
}

/// Write the data region and return the index that describes it.
///
/// `out` receives the bytes and must start empty, because positions are recorded as offsets
/// from zero. `out_file_number` is the number the output claims, and every copied entry is
/// rewritten to it.
pub fn write_data_region<W: Write, S: BlockSource>(
    plan: &MergePlan,
    source: &mut S,
    out: &mut W,
    out_file_number: u16,
) -> Result<DataRegion> {
    let mut at = 0u64;
    let mut seen: HashMap<(u16, i64, u32), i64> = HashMap::new();
    let mut duplicates_avoided = 0usize;
    let mut partitions = Vec::with_capacity(plan.partitions.len());

    for part in &plan.partitions {
        let reserved = copy_actions(
            &part.reserved,
            source,
            out,
            out_file_number,
            &mut at,
            &mut seen,
            &mut duplicates_avoided,
        )
        .with_context(|| {
            format!(
                "copying reserved sectors of disk {} partition {}",
                part.disk, part.partition
            )
        })?;

        let blocks = copy_actions(
            &part.blocks,
            source,
            out,
            out_file_number,
            &mut at,
            &mut seen,
            &mut duplicates_avoided,
        )
        .with_context(|| {
            format!(
                "copying data blocks of disk {} partition {}",
                part.disk, part.partition
            )
        })?;

        partitions.push(WrittenPartition {
            disk: part.disk,
            partition: part.partition,
            index: PartitionIndex {
                reserved,
                blocks: shape(plan, part, blocks)?,
            },
        });
    }

    let payload_bytes = at;
    // The metadata region starts on the next 4096-byte boundary. Measured on every file of
    // both test sets. An uncompressed set hides this, because its blocks land on the
    // boundary anyway and the gap reads as zero.
    let end = align_up(at);
    let padding = end - at;
    if padding > 0 {
        write_zeros(out, padding)?;
    }

    Ok(DataRegion {
        partitions,
        payload_bytes,
        end,
        duplicates_avoided,
    })
}

/// Copy one array of actions and return the index entries that describe the result.
#[allow(clippy::too_many_arguments)] // The alternative is a struct used in exactly one place.
fn copy_actions<W: Write, S: BlockSource>(
    actions: &[Action],
    source: &mut S,
    out: &mut W,
    out_file_number: u16,
    at: &mut u64,
    seen: &mut HashMap<(u16, i64, u32), i64>,
    duplicates_avoided: &mut usize,
) -> Result<Vec<DataBlockIndexElement>> {
    let mut entries = Vec::with_capacity(actions.len());
    for action in actions {
        let entry = match action {
            // A hole is written as all zeros. The reference restore skips it.
            Action::Hole => DataBlockIndexElement::default(),
            // The owning file stays on disk, so the reference is still good.
            Action::Keep(e) => *e,
            Action::Copy(e) => {
                let key = (e.file_number, e.file_position, e.block_length);
                let position = match seen.get(&key) {
                    Some(already) => {
                        *duplicates_avoided += 1;
                        *already
                    }
                    None => {
                        let bytes =
                            source.read_block(e.file_number, e.file_position, e.block_length)?;
                        ensure!(
                            bytes.len() as u32 == e.block_length,
                            "source returned {} bytes for a block of {}",
                            bytes.len(),
                            e.block_length
                        );
                        let position = i64::try_from(*at).context("output offset overflows")?;
                        out.write_all(&bytes)?;
                        *at += u64::from(e.block_length);
                        seen.insert(key, position);
                        position
                    }
                };
                DataBlockIndexElement {
                    file_position: position,
                    // Untouched. The hash covers the plaintext, which does not change, and
                    // the length is the stored length, which does not change either.
                    md5_hash: e.md5_hash,
                    block_length: e.block_length,
                    file_number: out_file_number,
                }
            }
        };
        entries.push(entry);
    }
    Ok(entries)
}

/// Put the block entries into the form the output's index uses.
fn shape(
    plan: &MergePlan,
    part: &PartitionPlan,
    entries: Vec<DataBlockIndexElement>,
) -> Result<Blocks> {
    if !plan.kind.delta_index() {
        return Ok(Blocks::Full(entries));
    }
    ensure!(
        part.positions.len() == entries.len(),
        "disk {} partition {} has {} delta entries but {} positions",
        part.disk,
        part.partition,
        entries.len(),
        part.positions.len()
    );
    Ok(Blocks::Delta(
        entries
            .into_iter()
            .zip(part.positions.iter().copied())
            .map(|(element, block_index)| DeltaDataBlock {
                element,
                block_index,
            })
            .collect(),
    ))
}

fn write_zeros<W: Write>(out: &mut W, mut count: u64) -> Result<()> {
    const CHUNK: usize = 8192;
    let zeros = [0u8; CHUNK];
    while count > 0 {
        let n = count.min(CHUNK as u64) as usize;
        out.write_all(&zeros[..n])?;
        count -= n as u64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{MergeKind, PartitionPlan};
    use std::collections::BTreeSet;

    /// A source whose bytes are derived from the request, so a copy can be tested without
    /// any file on disk.
    struct FakeSource {
        reads: Vec<(u16, i64, u32)>,
    }

    impl FakeSource {
        fn new() -> Self {
            Self { reads: Vec::new() }
        }

        /// The bytes a given block is expected to hold.
        fn expected(file_number: u16, position: i64, length: u32) -> Vec<u8> {
            (0..length)
                .map(|i| (file_number as u8) ^ (position as u8) ^ (i as u8))
                .collect()
        }
    }

    impl BlockSource for FakeSource {
        fn read_block(&mut self, file_number: u16, position: i64, length: u32) -> Result<Vec<u8>> {
            self.reads.push((file_number, position, length));
            Ok(Self::expected(file_number, position, length))
        }
    }

    fn element(file_number: u16, position: i64, length: u32) -> DataBlockIndexElement {
        DataBlockIndexElement {
            file_position: position,
            md5_hash: [file_number as u8; 16],
            block_length: length,
            file_number,
        }
    }

    fn plan_with(kind: MergeKind, blocks: Vec<Action>, positions: Vec<u32>) -> MergePlan {
        plan_with_reserved(kind, Vec::new(), blocks, positions)
    }

    fn plan_with_reserved(
        kind: MergeKind,
        reserved: Vec<Action>,
        blocks: Vec<Action>,
        positions: Vec<u32>,
    ) -> MergePlan {
        MergePlan {
            from: 0,
            to: 1,
            kind,
            absorbed: BTreeSet::from([0, 1]),
            partitions: vec![PartitionPlan {
                disk: 0,
                partition: 0,
                reserved,
                blocks,
                positions,
            }],
            out_file_number: 1,
            out_increment_number: 1,
            metadata_bytes: 0,
        }
    }

    fn write(plan: &MergePlan) -> (Vec<u8>, DataRegion, FakeSource) {
        let mut source = FakeSource::new();
        let mut out = Vec::new();
        let region = write_data_region(plan, &mut source, &mut out, 1).unwrap();
        (out, region, source)
    }

    #[test]
    fn copied_bytes_match_their_source_exactly() {
        let a = element(0, 1000, 64);
        let b = element(1, 2048, 128);
        let plan = plan_with(
            MergeKind::SyntheticFull,
            vec![Action::Copy(a), Action::Copy(b)],
            Vec::new(),
        );
        let (out, region, _) = write(&plan);

        assert_eq!(region.payload_bytes, 192);
        assert_eq!(&out[0..64], &FakeSource::expected(0, 1000, 64)[..]);
        assert_eq!(&out[64..192], &FakeSource::expected(1, 2048, 128)[..]);
    }

    #[test]
    fn the_hash_and_the_length_are_never_altered() {
        let a = element(0, 1000, 64);
        let plan = plan_with(MergeKind::SyntheticFull, vec![Action::Copy(a)], Vec::new());
        let (_, region, _) = write(&plan);

        let Blocks::Full(entries) = &region.partitions[0].index.blocks else {
            panic!("expected a full index");
        };
        assert_eq!(entries[0].md5_hash, a.md5_hash);
        assert_eq!(entries[0].block_length, a.block_length);
        // Only these two change.
        assert_eq!(entries[0].file_position, 0);
        assert_eq!(entries[0].file_number, 1);
    }

    #[test]
    fn a_hole_writes_nothing_and_stays_a_hole() {
        let plan = plan_with(
            MergeKind::SyntheticFull,
            vec![Action::Hole, Action::Copy(element(0, 0, 32)), Action::Hole],
            Vec::new(),
        );
        let (out, region, source) = write(&plan);

        assert_eq!(out.len(), 4096, "32 bytes of payload, padded");
        assert_eq!(region.payload_bytes, 32);
        assert_eq!(source.reads.len(), 1, "a hole is never read");

        let Blocks::Full(entries) = &region.partitions[0].index.blocks else {
            panic!("expected a full index");
        };
        assert_eq!(entries[0], DataBlockIndexElement::default());
        assert_eq!(entries[2], DataBlockIndexElement::default());
        assert_eq!(entries[1].file_position, 0);
    }

    #[test]
    fn a_kept_reference_is_written_through_untouched() {
        let kept = element(7, 9999, 64);
        let plan = plan_with(
            MergeKind::SyntheticFull,
            vec![Action::Keep(kept)],
            Vec::new(),
        );
        let (out, region, source) = write(&plan);

        assert!(source.reads.is_empty(), "a kept block is never read");
        assert!(
            out.is_empty(),
            "a kept block writes nothing, and 0 needs no padding"
        );
        let Blocks::Full(entries) = &region.partitions[0].index.blocks else {
            panic!("expected a full index");
        };
        assert_eq!(entries[0], kept, "file number and position both survive");
    }

    #[test]
    fn a_block_referenced_twice_is_copied_once() {
        let shared = element(0, 4096, 64);
        let plan = plan_with(
            MergeKind::SyntheticFull,
            vec![
                Action::Copy(shared),
                Action::Copy(element(0, 8192, 64)),
                Action::Copy(shared),
            ],
            Vec::new(),
        );
        let (out, region, source) = write(&plan);

        assert_eq!(source.reads.len(), 2, "the repeat is served from the map");
        assert_eq!(region.duplicates_avoided, 1);
        assert_eq!(region.payload_bytes, 128, "two blocks, not three");
        assert_eq!(out.len(), 4096);

        let Blocks::Full(entries) = &region.partitions[0].index.blocks else {
            panic!("expected a full index");
        };
        assert_eq!(
            entries[0].file_position, entries[2].file_position,
            "both entries point at the one copy"
        );
        assert_ne!(entries[0].file_position, entries[1].file_position);
    }

    #[test]
    fn two_blocks_that_differ_only_in_owner_are_both_copied() {
        // Same offset and length in two different files is not the same block.
        let plan = plan_with(
            MergeKind::SyntheticFull,
            vec![
                Action::Copy(element(0, 512, 64)),
                Action::Copy(element(1, 512, 64)),
            ],
            Vec::new(),
        );
        let (_, region, source) = write(&plan);
        assert_eq!(source.reads.len(), 2);
        assert_eq!(region.duplicates_avoided, 0);
    }

    #[test]
    fn the_region_ends_on_an_aligned_boundary() {
        for (length, expected) in [(1u32, 4096u64), (4096, 4096), (4097, 8192), (8192, 8192)] {
            let plan = plan_with(
                MergeKind::SyntheticFull,
                vec![Action::Copy(element(0, 0, length))],
                Vec::new(),
            );
            let (out, region, _) = write(&plan);
            assert_eq!(region.end, expected, "payload of {length}");
            assert_eq!(out.len() as u64, expected);
            assert_eq!(region.end % 4096, 0);
            // The padding is zeros, not leftover data.
            assert!(out[length as usize..].iter().all(|b| *b == 0));
        }
    }

    #[test]
    fn reserved_sectors_are_copied_before_the_data_blocks_of_their_partition() {
        let plan = plan_with_reserved(
            MergeKind::SyntheticFull,
            vec![Action::Copy(element(0, 0, 16))],
            vec![Action::Copy(element(0, 64, 32))],
            Vec::new(),
        );
        let (_, region, source) = write(&plan);

        assert_eq!(source.reads[0], (0, 0, 16), "reserved first");
        assert_eq!(source.reads[1], (0, 64, 32));
        assert_eq!(region.partitions[0].index.reserved.len(), 1);
        assert_eq!(region.partitions[0].index.reserved[0].file_position, 0);
        let Blocks::Full(entries) = &region.partitions[0].index.blocks else {
            panic!("expected a full index");
        };
        assert_eq!(entries[0].file_position, 16);
    }

    #[test]
    fn an_incremental_merge_produces_a_delta_index() {
        let plan = plan_with(
            MergeKind::IncrementalMerge,
            vec![
                Action::Copy(element(1, 0, 64)),
                Action::Copy(element(1, 64, 64)),
            ],
            vec![9, 40],
        );
        let (_, region, _) = write(&plan);

        let Blocks::Delta(deltas) = &region.partitions[0].index.blocks else {
            panic!("expected a delta index");
        };
        assert_eq!(deltas.len(), 2);
        assert_eq!(deltas[0].block_index, 9);
        assert_eq!(deltas[1].block_index, 40);
        assert_eq!(deltas[0].element.file_position, 0);
        assert_eq!(deltas[1].element.file_position, 64);
    }

    #[test]
    fn a_delta_plan_without_matching_positions_is_rejected() {
        let plan = plan_with(
            MergeKind::IncrementalMerge,
            vec![Action::Copy(element(1, 0, 64))],
            vec![9, 40],
        );
        let mut source = FakeSource::new();
        let mut out = Vec::new();
        let err = write_data_region(&plan, &mut source, &mut out, 1).unwrap_err();
        assert!(format!("{err:#}").contains("positions"), "{err:#}");
    }

    #[test]
    fn the_index_serializes_to_the_expected_length() {
        let plan = plan_with(
            MergeKind::SyntheticFull,
            vec![Action::Copy(element(0, 0, 64)), Action::Hole],
            Vec::new(),
        );
        let (_, region, _) = write(&plan);
        // Two counts of 4 bytes, no reserved entries, two 30-byte elements.
        assert_eq!(region.partitions[0].index.to_bytes().len(), 4 + 4 + 60);
    }
}
