//! Deciding what a merge does, before anything is written.
//!
//! A plan names, for every logical block of every partition, one of three outcomes: the
//! block is a hole, the block keeps its existing reference, or the block's bytes must be
//! copied into the output. Nothing here opens a file for writing, so a plan is safe to
//! build and print.
//!
//! The rule for which blocks move is the subtle part, and it is not a range over file
//! numbers. See [`absorbed_file_numbers`].

use std::collections::BTreeSet;
use std::path::PathBuf;

use anyhow::{bail, ensure, Context, Result};
use serde_json::Value;

use crate::block::{self, align_up};
use crate::index::{Blocks, DataBlockIndexElement};
use crate::reader::BackupFile;
use crate::set::BackupSet;

/// What a merge produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeKind {
    /// The From file carries the full index, so every block resolves into the merge. The
    /// output stands alone and carries a complete index.
    SyntheticFull,
    /// The From file is an Incremental. The output still references earlier files.
    IncrementalMerge,
}

impl MergeKind {
    /// The value written to `_auxiliary_data.backup_definition.consolidation_type`.
    pub fn consolidation_type(self) -> &'static str {
        match self {
            MergeKind::SyntheticFull => "synthetic_full",
            MergeKind::IncrementalMerge => "incremental_merge",
        }
    }

    /// Whether the output's `$INDEX` holds delta elements.
    pub fn delta_index(self) -> bool {
        matches!(self, MergeKind::IncrementalMerge)
    }
}

/// What happens to one logical block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// The block was never captured. The entry is written as zeros and nothing is copied.
    Hole,
    /// The owner survives the merge, so the entry is written unchanged.
    Keep(DataBlockIndexElement),
    /// The owner is absorbed, so the bytes move into the output.
    Copy(DataBlockIndexElement),
}

impl Action {
    /// The bytes this action moves. Zero for a hole and for a kept reference.
    pub fn bytes_to_copy(&self) -> u64 {
        match self {
            Action::Copy(e) => u64::from(e.block_length),
            _ => 0,
        }
    }

    pub fn is_copy(&self) -> bool {
        matches!(self, Action::Copy(_))
    }
}

/// The plan for one partition.
#[derive(Debug, Clone)]
pub struct PartitionPlan {
    pub disk: usize,
    pub partition: usize,
    /// FAT reserved sectors, taken from the newest member rather than flattened.
    pub reserved: Vec<Action>,
    /// One action per logical block position this output names.
    ///
    /// For a synthetic Full this is dense and covers the whole partition. For an
    /// incremental merge it holds only the positions the absorbed members changed, and
    /// `positions` gives the logical index of each.
    pub blocks: Vec<Action>,
    /// Logical block index of each entry in `blocks`. Empty for a synthetic Full, where
    /// the position is the array index.
    pub positions: Vec<u32>,
}

impl PartitionPlan {
    fn actions(&self) -> impl Iterator<Item = &Action> {
        self.reserved.iter().chain(self.blocks.iter())
    }
}

/// A complete merge plan.
#[derive(Debug, Clone)]
pub struct MergePlan {
    pub from: u16,
    pub to: u16,
    pub kind: MergeKind,
    /// Every file number whose bytes the output takes over. See [`absorbed_file_numbers`].
    pub absorbed: BTreeSet<u16>,
    pub partitions: Vec<PartitionPlan>,
    /// The file number the output claims.
    pub out_file_number: u16,
    /// The increment number the output claims.
    pub out_increment_number: u16,
    /// Bytes of metadata and footer, estimated from the To file. Crate-visible so that the
    /// writer's tests can build a plan without a backup set on disk.
    pub(crate) metadata_bytes: u64,
}

impl MergePlan {
    /// How many blocks move.
    pub fn blocks_to_copy(&self) -> usize {
        self.partitions
            .iter()
            .flat_map(PartitionPlan::actions)
            .filter(|a| a.is_copy())
            .count()
    }

    /// How many bytes move.
    pub fn bytes_to_copy(&self) -> u64 {
        self.partitions
            .iter()
            .flat_map(PartitionPlan::actions)
            .map(Action::bytes_to_copy)
            .sum()
    }

    /// How many blocks keep a reference to a file that stays on disk.
    pub fn blocks_kept(&self) -> usize {
        self.partitions
            .iter()
            .flat_map(PartitionPlan::actions)
            .filter(|a| matches!(a, Action::Keep(_)))
            .count()
    }

    /// How many positions are holes.
    pub fn holes(&self) -> usize {
        self.partitions
            .iter()
            .flat_map(PartitionPlan::actions)
            .filter(|a| matches!(a, Action::Hole))
            .count()
    }

    /// The size of the output file.
    ///
    /// The data region is the copied bytes, padded up to a 4096-byte boundary. The
    /// metadata estimate comes from the To file, whose blocks are copied verbatim, plus the
    /// index arrays this plan produces.
    pub fn projected_size(&self) -> u64 {
        align_up(self.bytes_to_copy()) + self.metadata_bytes
    }

    /// The file numbers whose files become redundant once the output is in place.
    pub fn redundant_file_numbers(&self) -> Vec<u16> {
        self.absorbed.iter().copied().collect()
    }
}

/// Every file number whose bytes the output takes over.
///
/// This is the part a range over file numbers gets wrong, and getting it wrong loses data
/// silently. Suppose an earlier run produced file 5 with `merged_files` of 3 and 4. Files 3
/// and 4 no longer exist, but index entries still name them, because that is exactly what
/// `merged_files` is for. Merge From 5 To 7, and a block tagged 3 fails a range test of 5
/// through 7. It is left as a reference, and then file 5, which physically holds its bytes,
/// is deleted.
///
/// So the set is the closure: every member in the range contributes its own number and
/// every number it already absorbed.
///
/// Members are selected by `increment_number` rather than by `file_number`, so that a split
/// continuation file, which shares its parent's increment, is swept in with its parent.
pub fn absorbed_file_numbers(set: &BackupSet, from: &BackupFile, to: &BackupFile) -> BTreeSet<u16> {
    closure_over(
        set.members
            .iter()
            .map(|m| (m.header.increment_number, m.header.owned_file_numbers())),
        from.header.increment_number,
        to.header.increment_number,
    )
}

/// The closure itself, over `(increment_number, owned file numbers)` pairs.
///
/// Split out from [`absorbed_file_numbers`] so that the rule can be tested directly. No
/// backup file in the test corpus has ever been consolidated, so the case this exists to
/// handle cannot be reached through real data yet.
fn closure_over(
    members: impl Iterator<Item = (u16, Vec<u16>)>,
    low: u16,
    high: u16,
) -> BTreeSet<u16> {
    let mut out = BTreeSet::new();
    for (increment, owned) in members {
        if increment >= low && increment <= high {
            out.extend(owned);
        }
    }
    out
}

/// Build a plan for merging `from` through `to`.
pub fn build(set: &BackupSet, from_number: u16, to_number: u16) -> Result<MergePlan> {
    let from = member_with_number(set, from_number)?;
    let to = member_with_number(set, to_number)?;
    check_rules(set, from, to)?;

    // The plan resolves the chain as of the newest member, so the To file has to be that
    // member. Discovering the set from a later file would flatten past the merge point and
    // plan against blocks the output must not contain.
    ensure!(
        to.header.file_number == set.newest().header.file_number,
        "the set was discovered as of file {}, but the To file is {}; \
         discover the set from the To file",
        set.newest().header.file_number,
        to_number
    );

    let absorbed = absorbed_file_numbers(set, from, to);
    let kind = if from.header.is_full_index() {
        MergeKind::SyntheticFull
    } else {
        MergeKind::IncrementalMerge
    };

    let flat = set.flatten()?;
    let mut partitions = Vec::new();

    for (d, disk) in to.disks.iter().enumerate() {
        for (p, part) in disk.partitions.iter().enumerate() {
            let resolved = flat
                .disks
                .get(d)
                .and_then(|disk| disk.get(p))
                .with_context(|| format!("the flattened set has no disk {d} partition {p}"))?;

            // Reserved sectors are never flattened. The newest member carries the whole
            // array, and every entry names that member, so every one of them moves.
            let reserved = part
                .index
                .reserved
                .iter()
                .map(|e| classify(*e, &absorbed))
                .collect();

            let (blocks, positions) = match kind {
                MergeKind::SyntheticFull => (
                    resolved.iter().map(|e| classify(*e, &absorbed)).collect(),
                    Vec::new(),
                ),
                MergeKind::IncrementalMerge => {
                    let positions = changed_positions(set, &absorbed, d, p);
                    let blocks = positions
                        .iter()
                        .map(|at| {
                            let entry = resolved.get(*at as usize).copied().unwrap_or_default();
                            classify(entry, &absorbed)
                        })
                        .collect();
                    (blocks, positions)
                }
            };

            partitions.push(PartitionPlan {
                disk: d,
                partition: p,
                reserved,
                blocks,
                positions,
            });
        }
    }

    let metadata_bytes = estimate_metadata(to, &partitions, kind)?;

    Ok(MergePlan {
        from: from_number,
        to: to_number,
        kind,
        absorbed,
        partitions,
        // The output claims the identity of the From file, which is what the original tool
        // leaves behind: it merges into that file and deletes the rest of the range. Macrium
        // Reflect X groups a set around it and shows a merged file that claims the last file
        // of the range under Orphan Files instead. Measured in 10.0.8843 on 2026-09-18.
        out_file_number: from.header.file_number,
        out_increment_number: from.header.increment_number,
        metadata_bytes,
    })
}

/// A hole stays a hole. An entry owned by an absorbed file moves. Anything else keeps its
/// reference to a file that stays on disk.
fn classify(entry: DataBlockIndexElement, absorbed: &BTreeSet<u16>) -> Action {
    if entry.is_hole() {
        Action::Hole
    } else if absorbed.contains(&entry.file_number) {
        Action::Copy(entry)
    } else {
        Action::Keep(entry)
    }
}

/// The logical positions that the absorbed members changed, in ascending order.
///
/// A position named by any absorbed member has to appear in the output, because the file
/// that recorded the change is going away.
fn changed_positions(
    set: &BackupSet,
    absorbed: &BTreeSet<u16>,
    disk: usize,
    partition: usize,
) -> Vec<u32> {
    let mut out = BTreeSet::new();
    for member in &set.members {
        if !absorbed.contains(&member.header.file_number) {
            continue;
        }
        let Some(part) = member
            .disks
            .get(disk)
            .and_then(|disk| disk.partitions.get(partition))
        else {
            continue;
        };
        if let Blocks::Delta(deltas) = &part.index.blocks {
            out.extend(deltas.iter().map(|d| d.block_index));
        }
    }
    out.into_iter().collect()
}

/// Estimate the metadata region and footer of the output.
///
/// The per-disk and per-partition blocks are copied verbatim, so their sizes are known
/// exactly. The index arrays are computed from this plan. The JSON changes length slightly
/// when it is patched, so its current length is used and a small allowance is added.
fn estimate_metadata(
    to: &BackupFile,
    partitions: &[PartitionPlan],
    kind: MergeKind,
) -> Result<u64> {
    let mut total = 0u64;

    for disk in &to.disks {
        let list = &disk.blocks;
        total += list.end - list.blocks[0].offset;
        for part in &disk.partitions {
            let (start, index_at) = part.span_before_index();
            total += index_at - start;
        }
    }

    let element = if kind.delta_index() {
        crate::index::DELTA_LEN
    } else {
        crate::index::ELEMENT_LEN
    };
    for plan in partitions {
        let payload =
            4 + plan.reserved.len() * crate::index::ELEMENT_LEN + 4 + plan.blocks.len() * element;
        total += block::HEADER_LEN as u64 + payload as u64;
    }

    // Root list: the JSON block, then $AUXDATA if the source has one.
    total += block::HEADER_LEN as u64 + to.json_raw.len() as u64;
    if let Some(aux) = to.root_list.find(block::AUXDATA) {
        total += block::HEADER_LEN as u64 + u64::from(aux.header.block_length);
    }
    // Patching lengthens the document a little. merged_files and a rewritten file_history
    // are the growth.
    total += 512;
    total += block::FOOTER_LEN;

    Ok(total)
}

fn member_with_number(set: &BackupSet, number: u16) -> Result<&BackupFile> {
    set.members
        .iter()
        .find(|m| m.header.file_number == number)
        .with_context(|| format!("no file of this backup set has file number {number}"))
}

/// The rules a merge has to satisfy before a plan means anything.
///
/// The wording of the first four comes from the original tool, so that existing habits and
/// scripts keep working. Note that it prints no trailing period on those.
fn check_rules(set: &BackupSet, from: &BackupFile, to: &BackupFile) -> Result<()> {
    check_pair(from, to)?;

    // Blocks are copied byte for byte, which is only sound when these agree across every
    // member. A set whose compression changed mid-chain cannot be merged this way, because
    // data blocks carry no per-block compression flag.
    for member in &set.members {
        settings_match(&member.json, &to.json).with_context(|| {
            format!(
                "{} does not match the rest of the set",
                member.path.display()
            )
        })?;
    }

    // A split set is a tool limit rather than a broken set. The continuation file holds
    // data blocks and no index of its own, and nothing here knows how to carry that shape
    // into an output.
    let split = split_in_range(
        set.members.iter().map(|m| {
            (
                m.header.increment_number,
                m.header.split_file,
                m.path.clone(),
            )
        }),
        from.header.increment_number,
        to.header.increment_number,
    );
    ensure!(
        split.is_empty(),
        "this range holds a split backup file, which this tool cannot merge yet: {}",
        split
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    Ok(())
}

/// The four rules that look only at the two files, in the wording of the original tool.
///
/// The command line runs these before it discovers the set. A set is discovered as of the To
/// file, so a From file newer than the To file is not a member at all, and without this the
/// run would report a missing file number rather than the documented message.
pub fn check_pair(from: &BackupFile, to: &BackupFile) -> Result<()> {
    if from.header.imageid != to.header.imageid {
        bail!("Error: From and To files are from a different backup set");
    }
    if from.header.increment_number > to.header.increment_number {
        bail!("Error: From file is more recent than the To file");
    }
    if from.header.is_differential() {
        bail!("Error: From file is a Differential");
    }
    if to.header.is_differential() {
        bail!("Error: To file is a Differential");
    }
    ensure!(
        from.header.file_number != to.header.file_number,
        "the From and To files are the same file, so there is nothing to merge"
    );
    Ok(())
}

/// Whether two documents agree on how their data blocks are stored.
fn settings_match(member: &Value, reference: &Value) -> Result<()> {
    let same = |path: &[&str]| -> bool {
        let pick = |doc: &Value| {
            path.iter()
                .try_fold(doc, |value, key| value.get(*key))
                .cloned()
                .unwrap_or(Value::Null)
        };
        pick(member) == pick(reference)
    };
    ensure!(
        same(&["_compression", "compression_level"])
            && same(&["_compression", "compression_method"]),
        "it uses different compression, so its blocks cannot be copied without re-encoding \
         them"
    );
    ensure!(
        same(&["_encryption", "enable"])
            && same(&["_encryption", "aes_type"])
            && same(&["_encryption", "key_iterations"]),
        "it uses different encryption, so its blocks cannot be copied without re-encrypting \
         them"
    );
    Ok(())
}

/// The split continuation files inside the increment range.
///
/// Split out from [`check_rules`] so the rule can be tested directly, because no file of the
/// test corpus is split.
fn split_in_range(
    members: impl Iterator<Item = (u16, bool, PathBuf)>,
    low: u16,
    high: u16,
) -> Vec<PathBuf> {
    members
        .filter(|(increment, split, _)| *split && *increment >= low && *increment <= high)
        .map(|(_, _, path)| path)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn element(file_number: u16, position: i64) -> DataBlockIndexElement {
        DataBlockIndexElement {
            file_position: position,
            md5_hash: [file_number as u8; 16],
            block_length: 64,
            file_number,
        }
    }

    fn absorbed(numbers: &[u16]) -> BTreeSet<u16> {
        numbers.iter().copied().collect()
    }

    #[test]
    fn a_hole_is_planned_as_a_hole_and_moves_nothing() {
        let action = classify(DataBlockIndexElement::default(), &absorbed(&[0, 1]));
        assert_eq!(action, Action::Hole);
        assert_eq!(action.bytes_to_copy(), 0);
        assert!(!action.is_copy());
    }

    #[test]
    fn a_block_owned_by_an_absorbed_file_moves() {
        let e = element(1, 4096);
        assert_eq!(classify(e, &absorbed(&[0, 1])), Action::Copy(e));
        assert_eq!(classify(e, &absorbed(&[0, 1])).bytes_to_copy(), 64);
    }

    #[test]
    fn a_block_owned_by_a_surviving_file_keeps_its_reference() {
        // File 0 sits before the From file, so its file stays on disk.
        let e = element(0, 512);
        assert_eq!(classify(e, &absorbed(&[1, 2])), Action::Keep(e));
        assert_eq!(classify(e, &absorbed(&[1, 2])).bytes_to_copy(), 0);
    }

    #[test]
    fn a_zero_length_block_is_a_hole_even_when_its_owner_is_absorbed() {
        // block_length of zero wins over ownership. Copying such an entry would move no
        // bytes but would still rewrite its file number, which is wrong.
        let mut e = element(1, 4096);
        e.block_length = 0;
        assert_eq!(classify(e, &absorbed(&[1])), Action::Hole);
    }

    #[test]
    fn the_closure_reduces_to_a_range_when_nothing_was_consolidated() {
        // Four plain files, merge increments 1 through 3.
        let members = vec![(0u16, vec![0u16]), (1, vec![1]), (2, vec![2]), (3, vec![3])];
        let got = closure_over(members.into_iter(), 1, 3);
        assert_eq!(got, absorbed(&[1, 2, 3]));
    }

    #[test]
    fn the_closure_sweeps_in_numbers_a_member_already_absorbed() {
        // This is the case a range over file numbers gets wrong. File 5 is the output of
        // an earlier merge and claims 3 and 4, whose files no longer exist. Index entries
        // elsewhere still name 3 and 4. Merging 5 through 7 must take all of them over,
        // because file 5 is what physically holds those bytes.
        let members = vec![
            (0u16, vec![0u16]),
            (1, vec![1]),
            (2, vec![2]),
            (5, vec![5, 3, 4]),
            (6, vec![6]),
            (7, vec![7]),
        ];
        let got = closure_over(members.into_iter(), 5, 7);
        assert_eq!(got, absorbed(&[3, 4, 5, 6, 7]));

        // The naive rule would have missed 3 and 4, and a block tagged with either one
        // would have been left pointing at a file that the merge deletes.
        let naive: BTreeSet<u16> = (5..=7).collect();
        assert!(!naive.contains(&3) && !naive.contains(&4));
    }

    #[test]
    fn a_block_from_a_previously_absorbed_file_is_planned_as_a_copy() {
        // The consequence of the test above, at the level of one block.
        let members = vec![(5u16, vec![5, 3, 4]), (6, vec![6])];
        let closure = closure_over(members.into_iter(), 5, 6);
        let orphan = element(3, 8192);
        assert_eq!(classify(orphan, &closure), Action::Copy(orphan));

        let naive: BTreeSet<u16> = (5..=6).collect();
        assert_eq!(classify(orphan, &naive), Action::Keep(orphan));
    }

    #[test]
    fn a_split_continuation_is_swept_in_with_its_parent() {
        // A split part shares its parent's increment number but has its own file number.
        // Selecting members by increment rather than by file number is what catches it.
        let members = vec![(0u16, vec![0u16]), (1, vec![1]), (1, vec![2]), (2, vec![3])];
        let got = closure_over(members.into_iter(), 1, 1);
        assert_eq!(got, absorbed(&[1, 2]));
    }

    /// A backup file with only the fields the refusal rules read.
    fn member(imageid: &str, file_number: u16, increment: u16, backup_type: &str) -> BackupFile {
        let json = serde_json::json!({ "disks": [] });
        BackupFile {
            path: PathBuf::from(format!("SET-{file_number:02}-{increment:02}.mrimgx")),
            size: 0,
            json_raw: Vec::new(),
            json,
            header: crate::json::Header {
                imageid: imageid.to_string(),
                file_number,
                increment_number: increment,
                merged_files: Vec::new(),
                split_file: false,
                index_file_position: 0,
                delta_index: true,
                backup_type: backup_type.to_string(),
            },
            root_at: 0,
            root_list: crate::block::BlockList {
                blocks: Vec::new(),
                end: 0,
            },
            disks: Vec::new(),
        }
    }

    #[test]
    fn each_refusal_uses_the_wording_of_the_original_tool() {
        let full = member("AAAA0000AAAA0000", 0, 0, "full");
        let incremental = member("AAAA0000AAAA0000", 1, 1, "inc");
        let message =
            |from: &BackupFile, to: &BackupFile| check_pair(from, to).unwrap_err().to_string();

        let other_set = member("BBBB1111BBBB1111", 1, 1, "inc");
        assert_eq!(
            message(&full, &other_set),
            "Error: From and To files are from a different backup set"
        );
        assert_eq!(
            message(&incremental, &full),
            "Error: From file is more recent than the To file"
        );
        let differential = member("AAAA0000AAAA0000", 1, 1, "diff");
        assert_eq!(
            message(&differential, &incremental),
            "Error: From file is a Differential"
        );
        assert_eq!(
            message(&full, &differential),
            "Error: To file is a Differential"
        );
        assert!(message(&full, &full).contains("nothing to merge"));
        // And a legal pair passes.
        check_pair(&full, &incremental).unwrap();
    }

    #[test]
    fn a_member_whose_compression_or_encryption_differs_is_refused() {
        let reference = serde_json::json!({
            "_compression": { "compression_level": "high", "compression_method": "zstd" },
            "_encryption": { "enable": true, "aes_type": "aes-128", "key_iterations": 600000 }
        });
        settings_match(&reference, &reference).unwrap();

        let mut other = reference.clone();
        other["_compression"]["compression_level"] = serde_json::json!("none");
        assert!(settings_match(&other, &reference)
            .unwrap_err()
            .to_string()
            .contains("different compression"));

        let mut other = reference.clone();
        other["_encryption"]["aes_type"] = serde_json::json!("aes-256");
        assert!(settings_match(&other, &reference)
            .unwrap_err()
            .to_string()
            .contains("different encryption"));
    }

    #[test]
    fn a_split_file_in_the_range_is_named() {
        // A split part shares its parent's increment number. No file of the test corpus is
        // split, so this rule is tested on its own.
        let members = || {
            [
                (0u16, false, PathBuf::from("SET-00-00.mrimgx")),
                (1, false, PathBuf::from("SET-01-01.mrimgx")),
                (1, true, PathBuf::from("SET-02-01.mrimgx")),
                (2, false, PathBuf::from("SET-03-02.mrimgx")),
            ]
            .into_iter()
        };

        assert_eq!(
            split_in_range(members(), 0, 2),
            vec![PathBuf::from("SET-02-01.mrimgx")]
        );
        // A range that stops before the split part is clean.
        assert!(split_in_range(members(), 0, 0).is_empty());
        assert!(split_in_range(members(), 2, 2).is_empty());
    }

    #[test]
    fn merge_kind_maps_to_the_documented_strings() {
        assert_eq!(
            MergeKind::SyntheticFull.consolidation_type(),
            "synthetic_full"
        );
        assert_eq!(
            MergeKind::IncrementalMerge.consolidation_type(),
            "incremental_merge"
        );
        assert!(!MergeKind::SyntheticFull.delta_index());
        assert!(MergeKind::IncrementalMerge.delta_index());
    }

    /// A plan assembled by hand, so the reporting can be tested without a backup set.
    fn plan_with(actions: Vec<Action>) -> MergePlan {
        MergePlan {
            from: 0,
            to: 1,
            kind: MergeKind::SyntheticFull,
            absorbed: absorbed(&[0, 1]),
            partitions: vec![PartitionPlan {
                disk: 0,
                partition: 0,
                reserved: Vec::new(),
                blocks: actions,
                positions: Vec::new(),
            }],
            out_file_number: 1,
            out_increment_number: 1,
            metadata_bytes: 1000,
        }
    }

    #[test]
    fn the_plan_reports_counts_and_sizes() {
        let plan = plan_with(vec![
            Action::Copy(element(1, 0)),
            Action::Copy(element(1, 64)),
            Action::Hole,
            Action::Keep(element(0, 128)),
        ]);
        assert_eq!(plan.blocks_to_copy(), 2);
        assert_eq!(plan.bytes_to_copy(), 128);
        assert_eq!(plan.holes(), 1);
        assert_eq!(plan.blocks_kept(), 1);
        // 128 bytes of data pads up to one 4096-byte boundary, then the metadata follows.
        assert_eq!(plan.projected_size(), 4096 + 1000);
    }

    #[test]
    fn an_empty_data_region_still_pads_to_nothing_rather_than_a_full_block() {
        let plan = plan_with(vec![Action::Hole, Action::Keep(element(0, 0))]);
        assert_eq!(plan.bytes_to_copy(), 0);
        assert_eq!(plan.projected_size(), 1000);
    }

    #[test]
    fn redundant_files_are_reported_in_ascending_order() {
        let plan = plan_with(vec![Action::Hole]);
        assert_eq!(plan.redundant_file_numbers(), vec![0, 1]);
    }
}
