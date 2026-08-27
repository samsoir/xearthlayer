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
/// Writes to a sibling temp file, flushes, fsyncs, then renames — and removes
/// the temp file on any failure. Without the fsync, a crash can persist the
/// rename while the data blocks are still in flight, promoting a half-written
/// file to the live path.
pub fn write_atomic<F>(path: &Path, write: F) -> io::Result<()>
where
    F: FnOnce(&mut BufWriter<File>) -> io::Result<()>,
{
    let tmp_path = path.with_extension("tmp");

    let outcome = (|| {
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
/// Failure to delete is logged and swallowed: we are already on the
/// regeneration path, and a stray file on disk is not worth aborting for.
pub fn discard(path: &Path, reason: &IntegrityError) {
    tracing::warn!(
        path = %path.display(),
        reason = ?reason,
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
