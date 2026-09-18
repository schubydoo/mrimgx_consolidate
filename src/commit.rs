//! Taking the lock, writing to a temporary file, and committing it into place.
//!
//! The order of the commit sequence is fixed: flush, close, rename, flush the directory,
//! then read the output back. None of it is decoration. The primary destination is a
//! network share, where a rename that returns an error may already have succeeded and a
//! directory flush does nothing at all. `scratch/tad.md` section 7.4 records the evidence.
//!
//! Nothing here writes to a source file. A source is opened read-only by the reader, and
//! this module only ever creates the lock, the temporary output, and the destination.

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, ensure, Context, Result};

/// The lock file name, which is the name `Consolidate.exe` uses.
pub const LOCK_NAME: &str = "merge_running";

/// A lock over one destination directory.
///
/// The file is created with the exclusive flag, so two runs cannot both hold it. Dropping
/// this value removes the file, which covers a clean exit and a reported failure alike. A
/// run that is killed leaves the file behind, and [`Lock::clear`] is how a person removes
/// it.
#[derive(Debug)]
pub struct Lock {
    path: PathBuf,
}

impl Lock {
    /// Take the lock, or report what holds it.
    ///
    /// `note` is written into the file so that a second run can print who is merging what.
    pub fn take(directory: &Path, note: &str) -> Result<Self> {
        let path = directory.join(LOCK_NAME);
        let mut file = match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                let held = std::fs::read_to_string(&path)
                    .unwrap_or_else(|error| format!("(the lock file cannot be read: {error})"));
                bail!(
                    "another merge holds the lock at {}. It says:\n{}\n\
                     If no merge is running, clear it with `consolidate --recover`.",
                    path.display(),
                    held.trim_end()
                );
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("creating the lock at {}", path.display()))
            }
        };

        let started = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();
        let body = format!("pid {}\nstarted {started}\n{note}\n", process::id());
        file.write_all(body.as_bytes())
            .and_then(|()| file.sync_all())
            .with_context(|| format!("writing the lock at {}", path.display()))?;

        Ok(Self { path })
    }

    /// Remove a lock left behind by a killed run. Reports whether one was there.
    pub fn clear(directory: &Path) -> Result<bool> {
        let path = directory.join(LOCK_NAME);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error).with_context(|| format!("removing {}", path.display())),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        // Nothing useful can be done with a failure here, and the recovery mode exists for
        // the case where the file survives.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// The prefix every temporary output carries.
const TEMP_PREFIX: &str = "macrium_consolidation_temp-";

/// Remove what a killed run left in `directory`: the lock, and any temporary output.
///
/// Returns the paths that were removed. A run that was killed leaves both behind on
/// purpose, because a partly written file that nobody notices is worse than one that is
/// named in a report.
pub fn clear_leftovers(directory: &Path) -> Result<Vec<PathBuf>> {
    let mut removed = Vec::new();
    if Lock::clear(directory)? {
        removed.push(directory.join(LOCK_NAME));
    }

    let entries =
        std::fs::read_dir(directory).with_context(|| format!("listing {}", directory.display()))?;
    for entry in entries {
        let path = entry
            .with_context(|| format!("reading an entry of {}", directory.display()))?
            .path();
        let is_leftover = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(TEMP_PREFIX));
        if is_leftover {
            std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
            removed.push(path);
        }
    }
    Ok(removed)
}

/// File system types that are network mounts.
const REMOTE: &[&str] = &[
    "9p",
    "afs",
    "beegfs",
    "ceph",
    "cifs",
    "davfs",
    "fuse.glusterfs",
    "fuse.s3fs",
    "fuse.sshfs",
    "glusterfs",
    "lustre",
    "ncpfs",
    "nfs",
    "nfs4",
    "smb2",
    "smb3",
    "smbfs",
];

/// File system types that are local disks.
const LOCAL: &[&str] = &[
    "apfs", "bcachefs", "btrfs", "exfat", "ext2", "ext3", "ext4", "f2fs", "hfs", "hfsplus", "jfs",
    "msdos", "ntfs", "ntfs3", "overlay", "reiserfs", "tmpfs", "ufs", "vfat", "xfs", "zfs",
];

/// What kind of file system the destination sits on.
///
/// This decides how much a successful rename is worth. On a local disk it is the whole
/// guarantee. On a network mount it is one report among several, and the read-back is what
/// the safety rests on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mount {
    Local {
        file_system: String,
    },
    Remote {
        file_system: String,
    },
    /// The type could not be read. Treated as a network mount, because that is the careful
    /// side of the guess.
    Unknown,
}

impl Mount {
    /// Classify the file system that holds `directory`.
    pub fn of(directory: &Path) -> Self {
        let Ok(mountinfo) = std::fs::read_to_string("/proc/self/mountinfo") else {
            // Every platform but Linux, where this file does not exist.
            return Mount::Unknown;
        };
        let Ok(resolved) = directory.canonicalize() else {
            return Mount::Unknown;
        };
        match file_system_of(&mountinfo, &resolved) {
            Some(file_system) => Mount::classify(&file_system),
            None => Mount::Unknown,
        }
    }

    fn classify(file_system: &str) -> Self {
        if REMOTE.contains(&file_system) {
            return Mount::Remote {
                file_system: file_system.to_string(),
            };
        }
        if LOCAL.contains(&file_system) {
            return Mount::Local {
                file_system: file_system.to_string(),
            };
        }
        Mount::Unknown
    }

    /// Whether the careful path applies. An unknown type counts as remote.
    pub fn is_remote(&self) -> bool {
        !matches!(self, Mount::Local { .. })
    }

    /// One line naming what this destination is.
    pub fn describe(&self) -> String {
        match self {
            Mount::Local { file_system } => format!("a local {file_system} file system"),
            Mount::Remote { file_system } => format!("a {file_system} network mount"),
            Mount::Unknown => "a file system this tool cannot name".to_string(),
        }
    }

    /// The guarantees that do not hold on this destination.
    ///
    /// A run prints these, because a person deciding whether to delete the source files
    /// deserves to know which of them the tool can stand behind.
    pub fn caveats(&self) -> Vec<String> {
        if !self.is_remote() {
            return Vec::new();
        }
        let mut lines = vec![
            "flushing the directory entry does nothing here, so it proves nothing".to_string(),
            "a rename error can report work that already succeeded, so both paths are read"
                .to_string(),
            "the read-back after the rename is the only confirmation this run trusts".to_string(),
        ];
        if matches!(self, Mount::Unknown) {
            lines.push(
                "the file system type could not be read, so the careful path is used".to_string(),
            );
        }
        lines
    }
}

/// The file system type of the mount point that holds `directory`.
///
/// `mountinfo` is the content of `/proc/self/mountinfo`. Its mount point is field five, and
/// its type is the first field after the ` - ` separator. The longest mount point that is a
/// prefix of the directory wins, because mounts nest.
fn file_system_of(mountinfo: &str, directory: &Path) -> Option<String> {
    let mut best: Option<(usize, String)> = None;
    for line in mountinfo.lines() {
        // A line this parser does not recognize is skipped, never fatal.
        let Some((left, right)) = line.split_once(" - ") else {
            continue;
        };
        let (Some(mount_point), Some(file_system)) = (
            left.split_whitespace().nth(4),
            right.split_whitespace().next(),
        ) else {
            continue;
        };
        if !directory.starts_with(mount_point) {
            continue;
        }
        if best
            .as_ref()
            .is_none_or(|(len, _)| mount_point.len() > *len)
        {
            best = Some((mount_point.len(), file_system.to_string()));
        }
    }
    best.map(|(_, file_system)| file_system)
}

/// Free space on the file system that holds `directory`, in bytes.
///
/// This is the space a person without special privileges can use, which is smaller than the
/// raw free space on a file system that reserves blocks for root.
pub fn free_space(directory: &Path) -> Result<u64> {
    let stat = rustix::fs::statvfs(directory)
        .with_context(|| format!("reading the free space of {}", directory.display()))?;
    Ok(stat.f_bavail.saturating_mul(stat.f_frsize))
}

/// The slack added to the space a merge needs.
///
/// The projected size is an estimate of the metadata, so it can be a little low. Sixteen
/// megabytes covers that and leaves the destination with room to write its own metadata.
const SPACE_MARGIN: u64 = 16 * 1024 * 1024;

/// The largest file a FAT32 volume can hold, one byte short of four gibibytes.
const FAT32_LIMIT: u64 = 4 * 1024 * 1024 * 1024 - 1;

/// Make sure that the destination can hold a file of `size`.
///
/// FAT32 is the case that matters. A merged output is usually larger than any file of the
/// set, so a merge that would work anywhere else fails part way through on FAT32. The
/// original tool refuses a FAT32 destination outright.
pub fn check_file_size_limit(mount: &Mount, size: u64) -> Result<()> {
    let file_system = match mount {
        Mount::Local { file_system } | Mount::Remote { file_system } => file_system.as_str(),
        Mount::Unknown => return Ok(()),
    };
    if !matches!(file_system, "vfat" | "msdos") {
        return Ok(());
    }
    ensure!(
        size <= FAT32_LIMIT,
        "the destination is a {file_system} volume, which cannot hold a file over \
         {FAT32_LIMIT} bytes, and the output would be {size} bytes"
    );
    Ok(())
}

/// Refuse before writing anything, rather than failing forty gigabytes in.
pub fn check_free_space(directory: &Path, needed: u64) -> Result<u64> {
    let available = free_space(directory)?;
    let wanted = needed.saturating_add(SPACE_MARGIN);
    ensure!(
        available >= wanted,
        "{} has {available} bytes free and the merge needs {wanted}, \
         which is {needed} for the output and {SPACE_MARGIN} of margin",
        directory.display()
    );
    Ok(available)
}

/// Whether the file system set space aside for the output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reservation {
    Made,
    /// The file system does not support it. Nothing is emulated.
    Unsupported,
}

/// A partly written output, sitting beside its destination under a temporary name.
///
/// The temporary file is removed when this value is dropped, unless it was committed. So an
/// error anywhere in the write leaves the destination directory as it was, apart from the
/// lock.
#[derive(Debug)]
pub struct TempOutput {
    file: Option<File>,
    temp: PathBuf,
    destination: PathBuf,
    committed: bool,
}

impl TempOutput {
    /// Create the temporary file in the directory that holds `destination`.
    ///
    /// The name carries a random component, so two runs cannot collide and a temporary file
    /// left by a killed run is never clobbered. The file sits in the destination directory,
    /// so the rename that follows stays on one file system and is therefore atomic.
    pub fn create(destination: &Path) -> Result<Self> {
        let directory = destination
            .parent()
            .with_context(|| format!("{} has no parent directory", destination.display()))?;
        let named = tempfile::Builder::new()
            .prefix(TEMP_PREFIX)
            .suffix(".tmp")
            .tempfile_in(directory)
            .with_context(|| format!("creating a temporary file in {}", directory.display()))?;
        // keep() hands over the file and its path, so this module decides when it goes.
        let (file, temp) = named.keep().context("keeping the temporary output")?;

        Ok(Self {
            file: Some(file),
            temp,
            destination: destination.to_path_buf(),
            committed: false,
        })
    }

    /// The file to write the output into.
    pub fn writer(&mut self) -> &mut File {
        self.file
            .as_mut()
            .expect("the file is taken only by commit")
    }

    pub fn temp_path(&self) -> &Path {
        &self.temp
    }

    /// Give the temporary file the permissions of `model`.
    ///
    /// A temporary file is created readable only by its owner. That is right while it is
    /// half written and wrong for a backup file on a share, which has to stay as readable
    /// as the files it replaces.
    pub fn take_permissions_from(&self, model: &Path) -> Result<()> {
        let permissions = std::fs::metadata(model)
            .with_context(|| format!("reading the permissions of {}", model.display()))?
            .permissions();
        std::fs::set_permissions(&self.temp, permissions)
            .with_context(|| format!("setting the permissions of {}", self.temp.display()))
    }

    /// Ask the file system to set `bytes` aside for this file.
    ///
    /// A reservation turns a full disk into a refusal before the copy starts. It is not
    /// supported everywhere, and an unsupported reservation is ignored rather than emulated
    /// by writing zeros, which would double the work for nothing.
    ///
    /// The call extends the file to `bytes`, so [`TempOutput::commit`] cuts it back to what
    /// was written.
    pub fn reserve(&self, bytes: u64) -> Reservation {
        let file = self.file.as_ref().expect("reserve runs before commit");
        match rustix::fs::fallocate(file, rustix::fs::FallocateFlags::empty(), 0, bytes) {
            Ok(()) => Reservation::Made,
            Err(_) => Reservation::Unsupported,
        }
    }

    /// Flush, close, rename, then flush the directory.
    ///
    /// `final_size` is the size the writer reported. The file on disk must match it: a
    /// reservation leaves the file longer, and a short write leaves it shorter, and the
    /// second of those is a failure.
    ///
    /// The destination exists when this returns. It has not been read back: that is the
    /// caller's next step, and it is the only confirmation worth trusting over a network
    /// mount.
    pub fn commit(mut self, mount: &Mount, final_size: u64) -> Result<Commit> {
        let file = self.file.take().expect("commit runs once");
        let mut report = Commit::default();

        let length = file
            .metadata()
            .with_context(|| format!("measuring {}", self.temp.display()))?
            .len();
        if length > final_size {
            // A reservation set the file longer than the write. Cut it back, which frees
            // the blocks it set aside.
            file.set_len(final_size).with_context(|| {
                format!("cutting {} back to {final_size} bytes", self.temp.display())
            })?;
        }
        ensure!(
            length >= final_size,
            "{} is {length} bytes and the writer reported {final_size}. \
             The write was cut short, so nothing is renamed into place",
            self.temp.display()
        );

        // A failed flush is never retried. The copy in the page cache is possibly gone, so
        // the only honest move is to abort and leave every source untouched.
        match file.sync_all() {
            Ok(()) => {}
            Err(error) if is_unsupported(&error) => {
                // On Apple targets sync_all asks for F_FULLFSYNC, which a share rejects.
                // That is not a failure, so fall back to the weaker flush and record that
                // the guarantee is weaker than it looks.
                file.sync_data().with_context(|| {
                    format!("flushing {} after a downgrade", self.temp.display())
                })?;
                report.flush_downgraded = true;
            }
            Err(error) => {
                return Err(error).with_context(|| format!("flushing {}", self.temp.display()))
            }
        }

        // Rust has no close that reports its result, and this crate contains no unsafe
        // code, so the file is closed by dropping it. The flush above reports a full disk,
        // and the read-back after the rename is what the safety rests on.
        drop(file);

        self.rename_into_place(mount, &mut report)?;
        self.committed = true;

        if let Some(directory) = self.destination.parent() {
            // The result is ignored on purpose. On NFS and SMB this call is a no-op that
            // returns success, so neither outcome is evidence of anything.
            flush_directory(directory);
        }
        Ok(report)
    }

    /// Rename the temporary file into place, allowing for how a network mount behaves.
    fn rename_into_place(&self, mount: &Mount, report: &mut Commit) -> Result<()> {
        const ATTEMPTS: usize = 4;
        let mut wait = Duration::from_millis(100);

        for attempt in 1..=ATTEMPTS {
            let error = match std::fs::rename(&self.temp, &self.destination) {
                Ok(()) => return Ok(()),
                Err(error) => error,
            };

            // A rename error does not prove the rename failed. RENAME is not idempotent,
            // and on NFS below 4.1 a lost reply can be replayed, so the server reports an
            // error for work it already did. Read both paths before concluding anything.
            if mount.is_remote() && self.rename_already_happened() {
                report.rename_error_was_wrong = true;
                return Ok(());
            }

            // SMB fails renames for reasons that never occur locally: a destination held
            // open without delete access, or an unreleased oplock. Both surface as these.
            // Unlinking the destination first would work around them and would throw away
            // the atomicity this whole sequence is paying for.
            let worth_retrying = matches!(
                error.kind(),
                ErrorKind::PermissionDenied | ErrorKind::ResourceBusy | ErrorKind::Interrupted
            );
            if !worth_retrying || attempt == ATTEMPTS {
                return Err(error).with_context(|| {
                    format!(
                        "renaming {} to {}",
                        self.temp.display(),
                        self.destination.display()
                    )
                });
            }

            report.rename_retries += 1;
            std::thread::sleep(wait);
            wait *= 2;
        }
        unreachable!("the loop returns on the last attempt")
    }

    /// Whether the rename already happened: the destination is there and the temporary name
    /// is gone.
    fn rename_already_happened(&self) -> bool {
        self.destination.exists() && !self.temp.exists()
    }
}

/// What the commit had to do, beyond the happy path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Commit {
    /// The strong flush was not supported here, so a weaker one was used.
    pub flush_downgraded: bool,
    /// How many times the rename was retried.
    pub rename_retries: usize,
    /// The rename reported an error for work it had already done.
    pub rename_error_was_wrong: bool,
}

/// Whether an error means the call itself is not supported here.
fn is_unsupported(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::Unsupported | ErrorKind::InvalidInput
    )
}

impl Drop for TempOutput {
    fn drop(&mut self) {
        if !self.committed {
            // A partly written output never survives under a name anything else reads.
            self.file.take();
            let _ = std::fs::remove_file(&self.temp);
        }
    }
}

/// Flush the directory entry, and count nothing on the result.
///
/// On NFS and SMB this call is a no-op that returns success, so success here is not evidence
/// of durability. An error is tolerated for the same reason. Opening a directory as a file
/// works on Unix and fails on Windows, which this tool does not target.
fn flush_directory(directory: &Path) {
    if let Ok(handle) = File::open(directory) {
        let _ = handle.sync_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn a_second_run_is_refused_and_the_lock_is_printed() {
        let dir = tempfile::tempdir().unwrap();
        let _held = Lock::take(dir.path(), "from 0 to 1").unwrap();

        let error = Lock::take(dir.path(), "from 0 to 1")
            .unwrap_err()
            .to_string();

        assert!(error.contains("another merge holds the lock"), "{error}");
        assert!(error.contains("from 0 to 1"), "{error}");
        assert!(error.contains("pid "), "{error}");
    }

    #[test]
    fn the_lock_is_released_when_it_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = {
            let lock = Lock::take(dir.path(), "note").unwrap();
            lock.path().to_path_buf()
        };

        assert!(!path.exists());
        // So a second run can take it.
        Lock::take(dir.path(), "note").unwrap();
    }

    #[test]
    fn clearing_reports_whether_a_stale_lock_was_there() {
        let dir = tempfile::tempdir().unwrap();
        std::mem::forget(Lock::take(dir.path(), "a killed run").unwrap());

        assert!(Lock::clear(dir.path()).unwrap(), "the stale lock was there");
        assert!(!Lock::clear(dir.path()).unwrap(), "and now it is not");
    }

    #[test]
    fn the_temp_file_sits_beside_the_destination_under_a_random_name() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("MERGED-00-00.mrimgx");

        let first = TempOutput::create(&destination).unwrap();
        let second = TempOutput::create(&destination).unwrap();

        // The same directory, so the rename cannot cross a file system.
        assert_eq!(first.temp_path().parent(), Some(dir.path()));
        assert_ne!(
            first.temp_path(),
            second.temp_path(),
            "two runs must not collide"
        );
        let name = first.temp_path().file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("macrium_consolidation_temp-"), "{name}");
        assert!(name.ends_with(".tmp"), "{name}");
        assert!(
            !destination.exists(),
            "nothing is written to the destination"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_output_takes_the_permissions_of_the_file_it_replaces() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let model = dir.path().join("SET-01-01.mrimgx");
        std::fs::write(&model, b"a source file").unwrap();
        std::fs::set_permissions(&model, std::fs::Permissions::from_mode(0o644)).unwrap();
        let output = TempOutput::create(&dir.path().join("MERGED-00-00.mrimgx")).unwrap();
        // A temporary file starts private to its owner.
        let before = std::fs::metadata(output.temp_path()).unwrap().permissions();
        assert_eq!(before.mode() & 0o777, 0o600);

        output.take_permissions_from(&model).unwrap();

        let after = std::fs::metadata(output.temp_path()).unwrap().permissions();
        assert_eq!(after.mode() & 0o777, 0o644);
    }

    #[test]
    fn committing_renames_the_temporary_file_into_place() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("MERGED-00-00.mrimgx");
        let mut output = TempOutput::create(&destination).unwrap();
        let temp = output.temp_path().to_path_buf();
        output.writer().write_all(b"the output").unwrap();

        let report = output.commit(&Mount::of(dir.path()), 10).unwrap();
        assert_eq!(
            report,
            Commit::default(),
            "the happy path needs no fallback"
        );

        assert!(!temp.exists(), "the temporary name is gone");
        let mut written = String::new();
        File::open(&destination)
            .unwrap()
            .read_to_string(&mut written)
            .unwrap();
        assert_eq!(written, "the output");
    }

    /// Two lines of a real `/proc/self/mountinfo`, with a share mounted below a local disk.
    const MOUNTINFO: &str = "\
25 30 0:23 / / rw,relatime shared:1 - ext4 /dev/sda1 rw
41 25 0:44 / /mnt/backups rw,relatime shared:22 - nfs4 nas:/volume1 rw,vers=4.1
47 41 0:51 / /mnt/backups/odd rw,relatime shared:9 - somethingnew /dev/x rw
a line this parser does not understand";

    #[test]
    fn the_longest_mount_point_decides_the_file_system() {
        let of = |path: &str| file_system_of(MOUNTINFO, Path::new(path));

        assert_eq!(of("/var/tmp").as_deref(), Some("ext4"));
        // The share is mounted below the root, so the longer prefix wins.
        assert_eq!(of("/mnt/backups/set").as_deref(), Some("nfs4"));
        assert_eq!(of("/mnt/backups/odd/set").as_deref(), Some("somethingnew"));
    }

    #[test]
    fn a_network_mount_is_treated_as_one_and_says_what_does_not_hold() {
        let remote = Mount::classify("nfs4");
        assert_eq!(
            remote,
            Mount::Remote {
                file_system: "nfs4".into()
            }
        );
        assert!(remote.is_remote());
        assert!(remote.describe().contains("nfs4"));
        assert_eq!(remote.caveats().len(), 3);
    }

    #[test]
    fn a_local_disk_carries_no_caveats() {
        let local = Mount::classify("ext4");

        assert!(!local.is_remote());
        assert!(local.caveats().is_empty());
    }

    #[test]
    fn a_file_system_this_tool_cannot_name_takes_the_careful_path() {
        let unknown = Mount::classify("somethingnew");

        assert_eq!(unknown, Mount::Unknown);
        assert!(unknown.is_remote(), "an unknown type is treated as remote");
        assert_eq!(unknown.caveats().len(), 4);
    }

    #[test]
    fn a_rename_that_already_happened_is_recognized() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("MERGED-00-00.mrimgx");
        let output = TempOutput::create(&destination).unwrap();
        assert!(!output.rename_already_happened(), "nothing has moved yet");

        std::fs::rename(output.temp_path(), &destination).unwrap();

        assert!(
            output.rename_already_happened(),
            "the destination is there and the temporary name is gone"
        );
    }

    #[test]
    fn an_output_too_large_for_a_fat32_destination_is_refused() {
        let fat = Mount::classify("vfat");
        let small = 3 * 1024 * 1024 * 1024;

        check_file_size_limit(&fat, small).unwrap();
        let error = check_file_size_limit(&fat, FAT32_LIMIT + 1)
            .unwrap_err()
            .to_string();
        assert!(error.contains("cannot hold a file over"), "{error}");

        // Every other destination carries no such limit.
        check_file_size_limit(&Mount::classify("ext4"), u64::MAX).unwrap();
        check_file_size_limit(&Mount::classify("nfs4"), u64::MAX).unwrap();
        check_file_size_limit(&Mount::Unknown, u64::MAX).unwrap();
    }

    #[test]
    fn a_merge_that_does_not_fit_is_refused_before_anything_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let available = free_space(dir.path()).unwrap();
        assert!(available > 0, "the test directory has free space");

        let error = check_free_space(dir.path(), available)
            .unwrap_err()
            .to_string();

        assert!(error.contains("and the merge needs"), "{error}");
        // A merge that fits, margin included, is allowed.
        check_free_space(dir.path(), 1024).unwrap();
    }

    #[test]
    fn a_reservation_is_cut_back_to_what_was_written() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("MERGED-00-00.mrimgx");
        let mut output = TempOutput::create(&destination).unwrap();
        // A file system that does not support this reports so and nothing is emulated.
        let reserved = output.reserve(4096);
        if reserved == Reservation::Made {
            let length = std::fs::metadata(output.temp_path()).unwrap().len();
            assert_eq!(length, 4096, "the reservation extends the file");
        }
        output.writer().write_all(b"the output").unwrap();

        output.commit(&Mount::of(dir.path()), 10).unwrap();

        assert_eq!(std::fs::metadata(&destination).unwrap().len(), 10);
    }

    #[test]
    fn a_write_that_was_cut_short_is_never_renamed_into_place() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("MERGED-00-00.mrimgx");
        let mut output = TempOutput::create(&destination).unwrap();
        output.writer().write_all(b"half").unwrap();

        // The writer reported more bytes than reached the disk.
        let error = output
            .commit(&Mount::of(dir.path()), 4096)
            .unwrap_err()
            .to_string();

        assert!(error.contains("was cut short"), "{error}");
        assert!(!destination.exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_rename_refused_for_permission_is_retried_with_backoff() {
        use std::os::unix::fs::PermissionsExt;

        let parent = tempfile::tempdir().unwrap();
        let locked = parent.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        let destination = locked.join("MERGED-00-00.mrimgx");
        let mut output = TempOutput::create(&destination).unwrap();
        output.writer().write_all(b"the output").unwrap();
        let temp = output.temp_path().to_path_buf();
        // A directory that cannot be written is how SMB's refusals look from here.
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();

        let started = std::time::Instant::now();
        let error = output
            .commit(
                &Mount::Remote {
                    file_system: "smb3".into(),
                },
                10,
            )
            .unwrap_err();
        let waited = started.elapsed();

        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(error.to_string().contains("renaming"), "{error}");
        // Three waits of 100, 200 and 400 milliseconds before the fourth attempt.
        assert!(
            waited >= Duration::from_millis(700),
            "the retries did not back off: {waited:?}"
        );
        assert!(!destination.exists(), "the destination never appeared");
        // The temporary file is still there, because a directory that refuses a rename
        // refuses an unlink too. A real run cannot reach this state: the temporary file
        // could not have been created in a directory that refuses writes.
        assert!(temp.exists());
    }

    #[test]
    fn dropping_an_uncommitted_output_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("MERGED-00-00.mrimgx");
        let temp = {
            let mut output = TempOutput::create(&destination).unwrap();
            output.writer().write_all(b"half a file").unwrap();
            output.temp_path().to_path_buf()
        };

        assert!(!temp.exists(), "the temporary file is removed");
        assert!(!destination.exists(), "and the destination never appeared");
    }
}
