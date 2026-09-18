//! Finding the backup sets in a directory and reporting what a merge would reclaim.
//!
//! This reads metadata only. Every number it reports comes from the block index and the
//! sizes of the files, so a scan of a terabyte of backups touches a few megabytes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::plan::{self, MergeKind};
use crate::reader::BackupFile;
use crate::set::BackupSet;

/// One range that could be merged, and what it would cost and save.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub from: u16,
    pub to: u16,
    pub kind: MergeKind,
    /// Bytes the merge would copy.
    pub moves: u64,
    /// Bytes on disk the merge would release, once the absorbed files are deleted.
    pub reclaims: u64,
}

/// One range that a merge refuses, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    pub from: u16,
    pub to: u16,
    pub reason: String,
}

/// One backup set, as a scan sees it.
#[derive(Debug, Clone)]
pub struct SetReport {
    pub imageid: String,
    /// The newest member, which is the file a merge is planned against.
    pub newest: PathBuf,
    pub members: usize,
    /// What the set occupies on disk.
    pub bytes: u64,
    /// Ranges worth merging, largest saving first. Empty when there is nothing to gain.
    pub candidates: Vec<Candidate>,
    /// Ranges that start at a member and cannot be merged, oldest From first, with the
    /// reason. A range left out with no word reads as an oversight.
    pub refused: Vec<Refused>,
    /// Why this set cannot be merged, when it cannot.
    pub problem: Option<String>,
}

/// A file the scan could not use, and why.
#[derive(Debug, Clone)]
pub struct Skipped {
    pub path: PathBuf,
    pub reason: String,
}

/// What a scan found.
#[derive(Debug, Clone, Default)]
pub struct Scan {
    pub sets: Vec<SetReport>,
    pub skipped: Vec<Skipped>,
}

/// The extensions a backup file carries.
const EXTENSIONS: &[&str] = &["mrimg", "mrimgx"];

/// Members of the same set that sit above `increment`, which a merge does not see.
///
/// A set is discovered as of the To file, so a file newer than it is not a member and the
/// plan never looks at it. Those files keep their own record of which file held each block,
/// and a merge cannot update it without writing to them. This finds them so a run can say
/// so.
pub fn members_above(directory: &Path, imageid: &str, increment: u16) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let is_backup = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()));
        if !is_backup {
            continue;
        }
        if let Ok(file) = BackupFile::open(&path, false) {
            if file.header.imageid == imageid && file.header.increment_number > increment {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

/// What a scan of one folder of a tree found.
#[derive(Debug, Clone)]
pub struct Folder {
    pub directory: PathBuf,
    pub found: Scan,
}

/// Folders a recursive scan does not enter. Each is a storage system's read-only view of
/// earlier snapshots of the folders around it, so entering one would report every set again
/// once per snapshot: `.zfs` on ZFS, `.snapshot` on NetApp and others, `#snapshot` on
/// Synology.
const SNAPSHOT_FOLDERS: &[&str] = &[".zfs", ".snapshot", "#snapshot"];

/// Scan `root` and every folder below it, in path order.
///
/// A backup set always lives in one folder, so this is the one-folder scan run on each.
/// Symbolic links to folders are not followed, so a link loop cannot trap the walk and a
/// link cannot lead out of the tree. A folder below the root that cannot be listed is
/// reported as skipped, and the walk carries on.
pub fn scan_tree(root: &Path) -> Result<Vec<Folder>> {
    let mut out = vec![Folder {
        directory: root.to_path_buf(),
        found: scan(root)?,
    }];
    let mut pending = subfolders(root);

    while let Some(directory) = pending.pop() {
        let found = match scan(&directory) {
            Ok(found) => found,
            Err(error) => Scan {
                skipped: vec![Skipped {
                    path: directory.clone(),
                    reason: format!("{error:#}"),
                }],
                ..Scan::default()
            },
        };
        pending.extend(subfolders(&directory));
        out.push(Folder { directory, found });
    }

    out.sort_by(|a, b| a.directory.cmp(&b.directory));
    Ok(out)
}

/// The real folders directly inside `directory`, leaving out links and snapshot folders.
fn subfolders(directory: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    entries
        .flatten()
        // file_type does not follow a symbolic link, so a link to a folder reads as a link.
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
        .filter(|entry| {
            !SNAPSHOT_FOLDERS
                .iter()
                .any(|name| entry.file_name() == std::ffi::OsStr::new(name))
        })
        .map(|entry| entry.path())
        .collect()
}

/// Report every backup set in `directory`.
pub fn scan(directory: &Path) -> Result<Scan> {
    let mut newest: BTreeMap<String, BackupFile> = BTreeMap::new();
    let mut found = Scan::default();

    let entries =
        std::fs::read_dir(directory).with_context(|| format!("listing {}", directory.display()))?;
    for entry in entries {
        let path = match entry {
            Ok(entry) => entry.path(),
            Err(error) => {
                found.skipped.push(Skipped {
                    path: directory.to_path_buf(),
                    reason: format!("{error}"),
                });
                continue;
            }
        };
        let is_backup = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()));
        if !path.is_file() || !is_backup {
            continue;
        }

        // Header only. A scan never loads a block index it does not need.
        match BackupFile::open(&path, false) {
            Ok(file) => {
                let key = file.header.imageid.clone();
                let keep = newest
                    .get(&key)
                    .is_none_or(|held| file.header.increment_number > held.header.increment_number);
                if keep {
                    newest.insert(key, file);
                }
            }
            // A directory holds all sorts of things. A file this crate cannot read is
            // reported and the scan carries on.
            Err(error) => found.skipped.push(Skipped {
                path,
                reason: format!("{error:#}"),
            }),
        }
    }

    for (imageid, target) in newest {
        found.sets.push(report(&imageid, &target.path));
    }
    Ok(found)
}

/// Look at one set and decide what a merge of it would be worth.
fn report(imageid: &str, newest: &Path) -> SetReport {
    let mut out = SetReport {
        imageid: imageid.to_string(),
        newest: newest.to_path_buf(),
        members: 0,
        bytes: 0,
        candidates: Vec::new(),
        refused: Vec::new(),
        problem: None,
    };

    let set = match BackupSet::discover(newest) {
        Ok(set) => set,
        Err(error) => {
            out.problem = Some(format!("{error:#}"));
            return out;
        }
    };
    out.members = set.members.len();
    out.bytes = set.members.iter().map(|m| m.size).sum();

    let last = set.newest().header.file_number;
    if out.members < 2 {
        out.problem = Some("the set is one file, so there is nothing to merge".to_string());
        return out;
    }

    // Every member older than the newest can start a range. A number with no member of its
    // own, which retention deleted or a merge absorbed, cannot.
    let mut starts: Vec<u16> = set
        .members
        .iter()
        .map(|m| m.header.file_number)
        .filter(|n| *n < last)
        .collect();
    starts.sort_unstable();
    starts.dedup();

    let mut refusal = None;
    for from in starts {
        match plan::build(&set, from, last) {
            Ok(plan) => {
                let absorbed: u64 = plan
                    .redundant_file_numbers()
                    .iter()
                    .filter_map(|n| set.owner(*n))
                    .map(|f| f.size)
                    .sum();
                out.candidates.push(Candidate {
                    from,
                    to: last,
                    kind: plan.kind,
                    moves: plan.bytes_to_copy(),
                    reclaims: absorbed.saturating_sub(plan.projected_size()),
                });
            }
            // When every range fails, the first reason is the reason the set cannot be
            // merged at all.
            Err(error) => {
                let reason = format!("{error:#}");
                refusal.get_or_insert(reason.clone());
                out.refused.push(Refused {
                    from,
                    to: last,
                    reason,
                });
            }
        }
    }

    if out.candidates.is_empty() {
        out.problem = refusal.or(Some("no range of this set can be merged".to_string()));
    } else {
        out.candidates
            .sort_by_key(|c| std::cmp::Reverse(c.reclaims));
    }
    out
}
