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
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};

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
            .prefix("macrium_consolidation_temp-")
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

    /// Flush, close, rename, then flush the directory.
    ///
    /// The destination exists when this returns. It has not been read back: that is the
    /// caller's next step, and it is the only confirmation worth trusting over a network
    /// mount.
    pub fn commit(mut self) -> Result<()> {
        let file = self.file.take().expect("commit runs once");

        // A failed flush is never retried. The copy in the page cache is possibly gone, so
        // the only honest move is to abort and leave every source untouched.
        file.sync_all()
            .with_context(|| format!("flushing {}", self.temp.display()))?;

        // Rust has no close that reports its result, and this crate contains no unsafe
        // code, so the file is closed by dropping it. The flush above reports a full disk,
        // and the read-back after the rename is what the safety rests on.
        drop(file);

        std::fs::rename(&self.temp, &self.destination).with_context(|| {
            format!(
                "renaming {} to {}",
                self.temp.display(),
                self.destination.display()
            )
        })?;
        self.committed = true;

        if let Some(directory) = self.destination.parent() {
            flush_directory(directory);
        }
        Ok(())
    }
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

        output.commit().unwrap();

        assert!(!temp.exists(), "the temporary name is gone");
        let mut written = String::new();
        File::open(&destination)
            .unwrap()
            .read_to_string(&mut written)
            .unwrap();
        assert_eq!(written, "the output");
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
