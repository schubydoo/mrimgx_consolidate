//! Backup set discovery and chain flattening.
//!
//! A backup set is a Full plus the Incrementals that follow it. Each file holds only the
//! blocks that changed, so restoring the state as of file N means resolving, for every
//! logical block of every partition, which file of the set holds the current bytes.
//!
//! Both halves follow the reference SDK exactly, because Macrium's own reader has to be
//! able to open whatever this crate writes. Discovery mirrors `createBackupSet` and
//! flattening mirrors `buildIndex` and `mapDeltaToFullIndex`, both in
//! `src/libs/file_reader/backup_set.cpp`.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{bail, ensure, Context, Result};

use crate::index::{Blocks, DataBlockIndexElement};
use crate::reader::BackupFile;

/// The members of one backup set, newest first.
#[derive(Debug)]
pub struct BackupSet {
    /// Sorted descending by `file_number`, so index 0 is the newest member. The reference
    /// reader depends on this order and so does [`BackupSet::flatten`].
    pub members: Vec<BackupFile>,
    /// Maps every file number the set can name to the member that holds those bytes. A
    /// consolidated member claims its own number and every number it absorbed.
    owners: HashMap<u16, usize>,
}

impl BackupSet {
    /// Discover the set that `path` belongs to, resolved as of that file.
    ///
    /// Members are found by scanning the directory that holds `path` for files with the
    /// same extension, then keeping those whose `imageid` matches and whose
    /// `increment_number` is at or below the target's. Filenames are never trusted: the
    /// JSON is authoritative.
    ///
    /// A file in the directory that fails to parse is skipped rather than fatal, because
    /// the directory may hold unrelated files. The reference reader does the same.
    pub fn discover(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let target = BackupFile::open(path, false)?;
        let extension = path
            .extension()
            .context("the backup file has no extension, so the set cannot be discovered")?
            .to_owned();
        let directory = path.parent().unwrap_or_else(|| Path::new("."));

        let mut members = Vec::new();
        let mut skipped = Vec::new();
        for entry in std::fs::read_dir(directory)
            .with_context(|| format!("listing {}", directory.display()))?
        {
            let entry = entry?;
            if !entry.file_type()?.is_file() || entry.path().extension() != Some(&extension) {
                continue;
            }
            // Read the header only first. This is the cheap filter, and it avoids loading
            // a large index for a file that turns out to belong to another set.
            let candidate = match BackupFile::open(entry.path(), false) {
                Ok(f) => f,
                Err(err) => {
                    skipped.push(format!("{}: {err:#}", entry.path().display()));
                    continue;
                }
            };
            if candidate.header.imageid != target.header.imageid
                || candidate.header.increment_number > target.header.increment_number
            {
                continue;
            }
            match BackupFile::open(entry.path(), true) {
                Ok(f) => members.push(f),
                Err(err) => skipped.push(format!("{}: {err:#}", entry.path().display())),
            }
        }

        ensure!(
            !members.is_empty(),
            "no readable files of backup set {} were found in {}{}",
            target.header.imageid,
            directory.display(),
            format_skipped(&skipped)
        );

        members.sort_by_key(|m| std::cmp::Reverse(m.header.file_number));

        let mut owners = HashMap::new();
        for (i, member) in members.iter().enumerate() {
            for number in member.header.owned_file_numbers() {
                owners.insert(number, i);
            }
        }

        let set = Self { members, owners };
        set.check_complete()?;
        Ok(set)
    }

    /// The newest member, which is the one the set is resolved as of.
    pub fn newest(&self) -> &BackupFile {
        &self.members[0]
    }

    /// The member holding the bytes that `file_number` names.
    pub fn owner(&self, file_number: u16) -> Option<&BackupFile> {
        self.owners.get(&file_number).map(|i| &self.members[*i])
    }

    /// The member that carries a complete index of its own, which is the base of the chain.
    ///
    /// A synthetic Full takes `disk_size` from this file, because an Incremental records the
    /// CHS product rather than the true device size.
    pub fn base(&self) -> Result<&BackupFile> {
        Ok(&self.members[self.base_index()?])
    }

    /// Make sure that the file numbers form a gapless run from zero.
    ///
    /// A missing middle file leaves the chain unresolvable, and the failure would
    /// otherwise appear much later as a block that points at a file nobody has.
    fn check_complete(&self) -> Result<()> {
        let highest = self.members[0].header.file_number;
        let missing: Vec<u16> = (0..=highest)
            .filter(|n| !self.owners.contains_key(n))
            .collect();
        if !missing.is_empty() {
            bail!(
                "Backup set is not complete. At least one file may be missing. \
                 (no file of set {} claims file number {})",
                self.members[0].header.imageid,
                missing
                    .iter()
                    .map(u16::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        Ok(())
    }

    /// Index of the newest member that carries a complete index of its own.
    ///
    /// Because members are sorted newest first, this scans forward to find the youngest
    /// non-delta file, which is the base the delta replay builds on. A split continuation
    /// carries no index at all and is never the base.
    ///
    /// The Full is identified this way, and never by `backup_type`, which records the
    /// backup definition's type rather than the file's role in the chain.
    fn base_index(&self) -> Result<usize> {
        self.members
            .iter()
            .position(|m| m.header.is_full_index())
            .context(
                "no file of this backup set carries a full block index, \
                 so the chain has no base to resolve against",
            )
    }

    /// Resolve every logical block of every partition as of the newest member.
    ///
    /// Returns one dense array per disk and partition, in the same array order the JSON
    /// uses. An entry with a `block_length` of zero is a hole: the block was never
    /// captured, and a restore writes nothing for it.
    pub fn flatten(&self) -> Result<Flattened> {
        let base = self.base_index()?;
        let shape = self.shape()?;
        let mut disks = Vec::with_capacity(shape.len());

        for (d, partition_count) in shape.iter().copied().enumerate() {
            let mut partitions = Vec::with_capacity(partition_count);
            for p in 0..partition_count {
                // Seed from the base file's own index.
                let mut blocks = match &self.members[base].disks[d].partitions[p].index.blocks {
                    Blocks::Full(v) => v.clone(),
                    Blocks::Delta(_) => {
                        bail!("the base file of the set carries a delta index, which cannot happen")
                    }
                };

                // Replay oldest to newest. The members vector is sorted newest first, so
                // walking indices downward from the base walks time forward.
                for i in (0..=base).rev() {
                    let member = &self.members[i];
                    if member.header.split_file {
                        continue;
                    }
                    let Blocks::Delta(deltas) = &member.disks[d].partitions[p].index.blocks else {
                        continue;
                    };
                    for delta in deltas {
                        let at = delta.block_index as usize;
                        if at >= blocks.len() {
                            // The partition grew. A hole is the correct filler for any
                            // position the delta list does not name.
                            blocks.resize(at + 1, DataBlockIndexElement::default());
                        }
                        blocks[at] = delta.element;
                    }
                }

                partitions.push(blocks);
            }
            disks.push(partitions);
        }

        Ok(Flattened { disks })
    }

    /// The number of partitions on each disk, taken from the newest member.
    ///
    /// Every member must agree, because the reference reader matches disks and partitions
    /// by array position rather than by number.
    fn shape(&self) -> Result<Vec<usize>> {
        let expected: Vec<usize> = self
            .newest()
            .disks
            .iter()
            .map(|d| d.partitions.len())
            .collect();
        for member in &self.members {
            if member.header.split_file {
                continue;
            }
            let actual: Vec<usize> = member.disks.iter().map(|d| d.partitions.len()).collect();
            ensure!(
                actual == expected,
                "{} has {} disks with partition counts {actual:?}, but the newest member has \
                 {expected:?}; the reference reader matches partitions by position, so a set \
                 whose shape changes mid-chain cannot be resolved",
                member.path.display(),
                actual.len()
            );
        }
        Ok(expected)
    }
}

/// A resolved backup set: `disks[d][p]` is the dense block array of one partition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flattened {
    pub disks: Vec<Vec<Vec<DataBlockIndexElement>>>,
}

impl Flattened {
    /// Total bytes of captured data, ignoring holes.
    pub fn stored_bytes(&self) -> u64 {
        self.blocks().map(|e| u64::from(e.block_length)).sum()
    }

    /// Every non-hole block of every partition.
    pub fn blocks(&self) -> impl Iterator<Item = &DataBlockIndexElement> {
        self.disks
            .iter()
            .flat_map(|d| d.iter())
            .flat_map(|p| p.iter())
            .filter(|e| !e.is_hole())
    }

    /// How many captured blocks each file of the set still supplies.
    ///
    /// This is what decides how much a consolidation has to copy: every block whose owner
    /// is about to be deleted must move into the output.
    pub fn blocks_per_file(&self) -> HashMap<u16, usize> {
        let mut out = HashMap::new();
        for e in self.blocks() {
            *out.entry(e.file_number).or_insert(0) += 1;
        }
        out
    }
}

fn format_skipped(skipped: &[String]) -> String {
    if skipped.is_empty() {
        return String::new();
    }
    format!("\n  skipped: {}", skipped.join("\n  skipped: "))
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

    #[test]
    fn stored_bytes_ignores_holes() {
        let f = Flattened {
            disks: vec![vec![vec![
                element(0, 0),
                DataBlockIndexElement::default(),
                element(1, 64),
            ]]],
        };
        assert_eq!(f.stored_bytes(), 128);
        assert_eq!(f.blocks().count(), 2);
    }

    #[test]
    fn blocks_per_file_counts_each_owner() {
        let f = Flattened {
            disks: vec![vec![
                vec![element(0, 0), element(0, 64)],
                vec![element(2, 0), DataBlockIndexElement::default()],
            ]],
        };
        let counts = f.blocks_per_file();
        assert_eq!(counts.get(&0), Some(&2));
        assert_eq!(counts.get(&2), Some(&1));
        assert_eq!(counts.len(), 2);
    }
}
