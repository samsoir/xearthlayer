//! Shared mechanics for reading and writing cache entries safely.

use super::IntegrityError;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;

/// The ceiling for any length a cache file claims: the file's own size.
///
/// Invariant 2 in one call. Nothing legitimate can require reading more bytes
/// than the file holds, so this rejects a corrupt length with no risk to a
/// valid entry, however large.
pub fn length_ceiling(file: &File) -> io::Result<u64> {
    Ok(file.metadata()?.len())
}

/// Durably replace a cache file.
///
/// Creates the parent directory if it does not already exist, then writes to
/// a sibling temp file, flushes, fsyncs, then renames — and removes the temp
/// file on any failure. Without the fsync, a crash can persist the rename
/// while the data blocks are still in flight, promoting a half-written file
/// to the live path.
///
/// Owning `create_dir_all` here (rather than leaving it to callers) is load-
/// bearing, not a convenience: the one thing that reliably creates
/// `~/.xearthlayer` is `logging.rs`, keyed off the user-settable
/// `logging.file` config value. A caller that assumed some other code path
/// had already created the directory would fail `ENOENT` on a fresh install
/// with a non-default log path.
pub fn write_atomic<F>(path: &Path, write: F) -> io::Result<()>
where
    F: FnOnce(&mut BufWriter<File>) -> io::Result<()>,
{
    let tmp_path = path.with_extension("tmp");

    let outcome = (|| {
        if let Some(parent) = tmp_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = File::create(&tmp_path)?;
        let mut writer = BufWriter::new(file);
        write(&mut writer)?;
        writer.flush()?;
        writer.get_ref().sync_all()?;
        Ok(())
    })();

    match outcome {
        Ok(()) => {
            std::fs::rename(&tmp_path, path)?;
            Ok(())
        }
        Err(err) => {
            let _ = std::fs::remove_file(&tmp_path);
            Err(err)
        }
    }
}

/// Remove a rejected cache entry, logging why.
///
/// `validator` is the [`CacheEntryValidator::name`](super::CacheEntryValidator::name)
/// of whichever validator rejected the entry, carried purely for the log
/// line — callers with no validator in the loop (the index caches, which
/// reject on a bincode/plausibility check instead) can pass any stable
/// label for the check that failed.
///
/// Failure to delete is logged and swallowed: we are already on the
/// regeneration path, and a stray file on disk is not worth aborting for.
pub fn discard(path: &Path, reason: &IntegrityError, validator: &str) {
    tracing::warn!(
        path = %path.display(),
        reason = ?reason,
        validator,
        "discarding corrupt cache entry"
    );

    if let Err(err) = std::fs::remove_file(path) {
        if err.kind() != io::ErrorKind::NotFound {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "failed to delete rejected cache entry"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn length_ceiling_is_the_file_size() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("entry");
        std::fs::write(&path, vec![0u8; 1234]).unwrap();

        let file = std::fs::File::open(&path).unwrap();
        assert_eq!(length_ceiling(&file).unwrap(), 1234);
    }

    #[test]
    fn write_atomic_leaves_no_temp_file_on_success() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("entry.cache");

        write_atomic(&path, |w| w.write_all(b"payload")).unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"payload");
        assert!(!path.with_extension("tmp").exists());
    }

    #[test]
    fn write_atomic_creates_a_missing_parent_directory() {
        let temp = TempDir::new().unwrap();
        // Neither `nested` nor `deeper` exists yet — write_atomic must
        // create the whole path, not just assume it's there. This is what
        // makes `IndexCache::save` safe on a fresh install whose
        // `logging.file` points somewhere other than `~/.xearthlayer`
        // (I2/#253): nothing else on that path guarantees the directory
        // exists.
        let path = temp.path().join("nested/deeper/entry.cache");

        write_atomic(&path, |w| w.write_all(b"payload")).unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"payload");
    }

    #[test]
    fn write_atomic_removes_the_temp_file_when_the_write_fails() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("entry.cache");

        let result = write_atomic(&path, |_| {
            Err(std::io::Error::other("simulated write failure"))
        });

        assert!(result.is_err());
        assert!(
            !path.with_extension("tmp").exists(),
            "temp file must not survive"
        );
        assert!(!path.exists(), "a failed write must not promote anything");
    }
}
