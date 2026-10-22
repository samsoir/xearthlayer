//! Archive extraction for package installation.
//!
//! A package arrives as a `tar.gz` split into parts. This module joins the
//! parts back into one archive and unpacks it, all in process: no `tar`
//! binary is needed, so installs work wherever the client runs (#274).
//!
//! Decompression goes through flate2's `zlib-rs` backend, which picks SIMD
//! instructions (AVX2, SSE, NEON) at run time. One binary is therefore fast
//! on every CPU, which a build-time `target-cpu` flag could not achieve for
//! the packaged releases.

use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};

use flate2::read::GzDecoder;

use super::cancel::CancelToken;
use super::error::{ManagerError, ManagerResult};
use super::traits::ArchiveExtractor;

/// Read buffer for the compressed stream. Large enough that inflate, not
/// syscalls, sets the pace.
const READ_BUFFER_BYTES: usize = 1 << 20;

/// Extracts `tar.gz` package archives in process.
#[derive(Debug, Default)]
pub struct TarGzExtractor;

impl TarGzExtractor {
    /// Create a new extractor.
    pub fn new() -> Self {
        Self
    }

    /// Reassemble split archive parts into a single file by concatenating
    /// them in order.
    ///
    /// # Arguments
    ///
    /// * `parts` - Paths to the archive parts in order (e.g., .aa, .ab, .ac)
    /// * `output` - Path for the reassembled archive
    /// * `cancel` - Checked once per buffer; a set token ends the copy with
    ///   [`ManagerError::Cancelled`]
    ///
    /// # Returns
    ///
    /// The total size of the reassembled archive in bytes.
    pub fn reassemble(
        &self,
        parts: &[PathBuf],
        output: &Path,
        cancel: &CancelToken,
    ) -> ManagerResult<u64> {
        if parts.is_empty() {
            return Err(ManagerError::ExtractionFailed {
                path: output.to_path_buf(),
                reason: "No parts provided for reassembly".to_string(),
            });
        }

        // Verify all parts exist
        for part in parts {
            if !part.exists() {
                return Err(ManagerError::ReadFailed {
                    path: part.clone(),
                    source: io::Error::new(io::ErrorKind::NotFound, "Part file not found"),
                });
            }
        }

        // Create parent directory if needed
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent).map_err(|e| ManagerError::CreateDirFailed {
                path: parent.to_path_buf(),
                source: e,
            })?;
        }

        let output_file = File::create(output).map_err(|e| ManagerError::WriteFailed {
            path: output.to_path_buf(),
            source: e,
        })?;

        let mut total_size = 0u64;
        let mut writer = io::BufWriter::new(output_file);

        for part_path in parts {
            let file = File::open(part_path).map_err(|e| ManagerError::ReadFailed {
                path: part_path.clone(),
                source: e,
            })?;

            let mut reader = BufReader::new(file);
            let mut buffer = vec![0u8; 64 * 1024]; // 64KB buffer

            loop {
                if cancel.is_cancelled() {
                    return Err(ManagerError::Cancelled);
                }
                let bytes_read =
                    reader
                        .read(&mut buffer)
                        .map_err(|e| ManagerError::ReadFailed {
                            path: part_path.clone(),
                            source: e,
                        })?;

                if bytes_read == 0 {
                    break;
                }

                writer
                    .write_all(&buffer[..bytes_read])
                    .map_err(|e| ManagerError::WriteFailed {
                        path: output.to_path_buf(),
                        source: e,
                    })?;

                total_size += bytes_read as u64;
            }
        }

        writer.flush().map_err(|e| ManagerError::WriteFailed {
            path: output.to_path_buf(),
            source: e,
        })?;

        Ok(total_size)
    }

    /// Extract `archive_path` into `dest_dir`, stopping with
    /// [`ManagerError::Cancelled`] before the next entry once `cancel` is set.
    ///
    /// Returns the number of regular files extracted.
    pub fn extract_cancellable(
        &self,
        archive_path: &Path,
        dest_dir: &Path,
        cancel: &CancelToken,
    ) -> ManagerResult<usize> {
        let file = File::open(archive_path).map_err(|e| ManagerError::ReadFailed {
            path: archive_path.to_path_buf(),
            source: e,
        })?;
        self.unpack(file, archive_path, dest_dir, cancel)
    }

    /// Extract a split archive straight from its `parts`, in order, into
    /// `dest_dir`, without first joining them into one file.
    ///
    /// Each part is deleted as soon as it has been read to its end, so the
    /// disk never holds the parts and a joined copy together. A part that has
    /// not been fully read when this returns (an error or a cancel) is left
    /// in place. Stops with [`ManagerError::Cancelled`] before the next entry
    /// once `cancel` is set.
    ///
    /// Returns the number of regular files extracted.
    pub fn extract_parts(
        &self,
        parts: &[PathBuf],
        dest_dir: &Path,
        cancel: &CancelToken,
    ) -> ManagerResult<usize> {
        let Some(first) = parts.first() else {
            return Err(ManagerError::ExtractionFailed {
                path: dest_dir.to_path_buf(),
                reason: "No parts provided".to_string(),
            });
        };
        let reader = PartsReader::new(parts)?;
        self.unpack(reader, first, dest_dir, cancel)
    }

    /// Unpack the `tar.gz` stream `reader` into `dest_dir`. `source` names
    /// the archive in error messages and is never opened.
    fn unpack(
        &self,
        reader: impl Read,
        source: &Path,
        dest_dir: &Path,
        cancel: &CancelToken,
    ) -> ManagerResult<usize> {
        let failed = |reason: String| ManagerError::ExtractionFailed {
            path: source.to_path_buf(),
            reason,
        };

        fs::create_dir_all(dest_dir).map_err(|e| ManagerError::CreateDirFailed {
            path: dest_dir.to_path_buf(),
            source: e,
        })?;

        let gz = GzDecoder::new(BufReader::with_capacity(READ_BUFFER_BYTES, reader));
        let mut archive = tar::Archive::new(gz);

        let mut files = 0;
        for entry in archive.entries().map_err(|e| failed(e.to_string()))? {
            if cancel.is_cancelled() {
                return Err(ManagerError::Cancelled);
            }
            let mut entry = entry.map_err(|e| failed(e.to_string()))?;
            let name = entry
                .path()
                .map_err(|e| failed(e.to_string()))?
                .into_owned();
            let is_file = entry.header().entry_type().is_file();

            // `unpack_in` declines, rather than fails, an entry that would
            // land outside `dest_dir`. Ignoring that would install a package
            // with a file silently missing, so treat it as the corrupt or
            // hostile archive it is. System tar refused it too.
            let inside = entry
                .unpack_in(dest_dir)
                .map_err(|e| failed(format!("{}: {}", name.display(), e)))?;
            if !inside {
                return Err(failed(format!(
                    "entry {} would extract outside the destination",
                    name.display()
                )));
            }
            if is_file {
                files += 1;
            }
        }

        // tar stops at its end-of-archive marker and gzip at its trailer, so
        // neither reads the source to its end. Drain both: the gzip trailer's
        // checksum is verified on the way, and a split archive's last part
        // reaches EOF and is deleted.
        let mut gz = archive.into_inner();
        io::copy(&mut gz, &mut io::sink()).map_err(|e| failed(e.to_string()))?;
        io::copy(&mut gz.into_inner(), &mut io::sink()).map_err(|e| failed(e.to_string()))?;

        Ok(files)
    }
}

/// The parts of a split archive read as one stream, in order.
///
/// Each part is deleted as soon as it has been read to its end: gzip and tar
/// are streams, so nothing later needs it, and the temp filesystem then holds
/// only what is still unread. Parts are checksum-verified when they download,
/// so streaming them loses nothing that reassembling into one file gave.
struct PartsReader {
    pending: VecDeque<PathBuf>,
    current: Option<(PathBuf, File)>,
}

impl PartsReader {
    /// Refuses a missing part before anything is read or deleted.
    fn new(parts: &[PathBuf]) -> ManagerResult<Self> {
        if let Some(missing) = parts.iter().find(|p| !p.exists()) {
            return Err(ManagerError::ReadFailed {
                path: missing.clone(),
                source: io::Error::new(io::ErrorKind::NotFound, "Part file not found"),
            });
        }
        Ok(Self {
            pending: parts.iter().cloned().collect(),
            current: None,
        })
    }
}

impl Read for PartsReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.current.is_none() {
                let Some(path) = self.pending.pop_front() else {
                    return Ok(0);
                };
                let file = File::open(&path)?;
                self.current = Some((path, file));
            }
            if let Some((_, file)) = self.current.as_mut() {
                let n = file.read(buf)?;
                if n > 0 || buf.is_empty() {
                    return Ok(n);
                }
            }
            // End of this part: close it, delete it, move to the next.
            if let Some((path, file)) = self.current.take() {
                drop(file);
                if let Err(e) = fs::remove_file(&path) {
                    tracing::warn!(
                        part = %path.display(),
                        error = %e,
                        "could not delete a consumed archive part"
                    );
                }
            }
        }
    }
}

impl ArchiveExtractor for TarGzExtractor {
    fn extract(&self, archive_path: &Path, dest_dir: &Path) -> ManagerResult<usize> {
        self.extract_cancellable(archive_path, dest_dir, &CancelToken::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn test_tar_gz_extractor_new() {
        let extractor = TarGzExtractor::new();
        assert!(format!("{:?}", extractor).contains("TarGzExtractor"));
    }

    #[test]
    fn test_reassemble_empty_parts() {
        let temp = TempDir::new().unwrap();
        let extractor = TarGzExtractor::new();
        let output = temp.path().join("output.tar.gz");

        let result = extractor.reassemble(&[], &output, &CancelToken::new());
        assert!(result.is_err());
    }

    #[test]
    fn test_reassemble_missing_part() {
        let temp = TempDir::new().unwrap();
        let extractor = TarGzExtractor::new();
        let output = temp.path().join("output.tar.gz");
        let missing = temp.path().join("missing.part");

        let result = extractor.reassemble(&[missing], &output, &CancelToken::new());
        assert!(result.is_err());
    }

    #[test]
    fn test_reassemble_single_part() {
        let temp = TempDir::new().unwrap();
        let extractor = TarGzExtractor::new();

        // Create a test part file
        let part = temp.path().join("test.part");
        let mut file = File::create(&part).unwrap();
        file.write_all(b"test content").unwrap();

        let output = temp.path().join("output.bin");
        let size = extractor
            .reassemble(&[part], &output, &CancelToken::new())
            .unwrap();

        assert_eq!(size, 12); // "test content".len()
        assert!(output.exists());

        let content = fs::read_to_string(&output).unwrap();
        assert_eq!(content, "test content");
    }

    #[test]
    fn test_reassemble_multiple_parts() {
        let temp = TempDir::new().unwrap();
        let extractor = TarGzExtractor::new();

        // Create test part files
        let part1 = temp.path().join("test.aa");
        let part2 = temp.path().join("test.ab");
        let part3 = temp.path().join("test.ac");

        fs::write(&part1, b"Hello").unwrap();
        fs::write(&part2, b" ").unwrap();
        fs::write(&part3, b"World").unwrap();

        let output = temp.path().join("output.bin");
        let size = extractor
            .reassemble(&[part1, part2, part3], &output, &CancelToken::new())
            .unwrap();

        assert_eq!(size, 11); // "Hello World".len()

        let content = fs::read_to_string(&output).unwrap();
        assert_eq!(content, "Hello World");
    }

    /// Write a tar.gz at `out`. A name ending in `/` is a directory. Names
    /// are written into the header raw, because `tar::Builder` refuses a `..`
    /// component and one test needs exactly that.
    fn tar_gz(out: &Path, entries: &[(&str, &[u8])]) {
        let file = File::create(out).unwrap();
        let gz = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
        let mut builder = tar::Builder::new(gz);
        for (name, data) in entries {
            let mut header = tar::Header::new_gnu();
            header.as_old_mut().name[..name.len()].copy_from_slice(name.as_bytes());
            header.set_size(data.len() as u64);
            if name.ends_with('/') {
                header.set_entry_type(tar::EntryType::Directory);
                header.set_mode(0o755);
            } else {
                header.set_entry_type(tar::EntryType::Regular);
                header.set_mode(0o644);
            }
            header.set_cksum();
            builder.append(&header, *data).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap();
    }

    #[test]
    fn extract_unpacks_every_file_and_counts_only_files() {
        let temp = TempDir::new().unwrap();
        let archive = temp.path().join("pkg.tar.gz");
        tar_gz(
            &archive,
            &[
                ("pkg/", b""),
                ("pkg/a.txt", b"alpha"),
                ("pkg/sub/", b""),
                ("pkg/sub/b.txt", b"beta"),
                ("pkg/sub/c.dds", &[0, 1, 2, 255]),
            ],
        );
        let dest = temp.path().join("dest");

        let files = TarGzExtractor::new().extract(&archive, &dest).unwrap();

        assert_eq!(files, 3);
        assert_eq!(fs::read(dest.join("pkg/a.txt")).unwrap(), b"alpha");
        assert_eq!(fs::read(dest.join("pkg/sub/b.txt")).unwrap(), b"beta");
        assert_eq!(
            fs::read(dest.join("pkg/sub/c.dds")).unwrap(),
            [0, 1, 2, 255]
        );
    }

    #[test]
    fn extract_refuses_an_entry_that_escapes_the_destination() {
        // System tar strips the `../` and extracts the file anyway. A package
        // carrying such an entry is malformed or hostile, so refuse it.
        let temp = TempDir::new().unwrap();
        let archive = temp.path().join("pkg.tar.gz");
        tar_gz(
            &archive,
            &[("pkg/ok.txt", b"ok"), ("../escape.txt", b"gotcha")],
        );
        let dest = temp.path().join("dest");

        let err = TarGzExtractor::new()
            .extract(&archive, &dest)
            .expect_err("an escaping entry must fail the extraction");

        assert!(err.to_string().contains("escape.txt"), "{err}");
        assert!(!temp.path().join("escape.txt").exists());
        assert!(!dest.join("escape.txt").exists());
    }

    #[test]
    fn a_corrupt_archive_is_an_error_naming_the_archive() {
        let temp = TempDir::new().unwrap();
        let archive = temp.path().join("broken.tar.gz");
        fs::write(&archive, b"this is not a gzip stream").unwrap();

        let err = TarGzExtractor::new()
            .extract(&archive, &temp.path().join("dest"))
            .expect_err("garbage must not extract");

        assert!(err.to_string().contains("broken.tar.gz"), "{err}");
    }

    #[test]
    fn a_cancelled_reassembly_stops_and_reports_cancelled() {
        let temp = TempDir::new().unwrap();
        let part = temp.path().join("a.aa");
        fs::write(&part, vec![0u8; 256 * 1024]).unwrap();
        let cancel = CancelToken::new();
        cancel.cancel();

        let result = TarGzExtractor::new().reassemble(&[part], &temp.path().join("out"), &cancel);

        assert!(matches!(result, Err(ManagerError::Cancelled)), "{result:?}");
    }

    #[test]
    fn a_cancelled_extraction_stops_before_the_next_entry() {
        let temp = TempDir::new().unwrap();
        let archive = temp.path().join("pkg.tar.gz");
        tar_gz(&archive, &[("pkg/a.txt", b"alpha"), ("pkg/b.txt", b"beta")]);
        let dest = temp.path().join("dest");
        let cancel = CancelToken::new();
        cancel.cancel();

        let result = TarGzExtractor::new().extract_cancellable(&archive, &dest, &cancel);

        assert!(matches!(result, Err(ManagerError::Cancelled)), "{result:?}");
        assert!(!dest.join("pkg/a.txt").exists());
    }

    /// Split `bytes` into files of `size` bytes in `dir`, named like `split`.
    fn split_into(dir: &Path, bytes: &[u8], size: usize) -> Vec<PathBuf> {
        bytes
            .chunks(size)
            .enumerate()
            .map(|(i, chunk)| {
                let path = dir.join(format!("pkg.tar.gz.a{}", (b'a' + i as u8) as char));
                fs::write(&path, chunk).unwrap();
                path
            })
            .collect()
    }

    #[test]
    fn the_parts_reader_yields_the_parts_concatenated_and_deletes_each_once_read() {
        let temp = TempDir::new().unwrap();
        let parts = split_into(temp.path(), b"abcdefghij", 3); // abc def ghi j
        let mut reader = PartsReader::new(&parts).unwrap();
        let mut first = [0u8; 4];
        reader.read_exact(&mut first).unwrap();
        assert_eq!(&first, b"abcd");
        assert!(!parts[0].exists(), "a part read to its end is deleted");
        assert!(parts[1].exists(), "a part still being read stays");

        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, b"efghij");
        assert!(parts.iter().all(|p| !p.exists()));
    }

    #[test]
    fn a_missing_part_is_refused_before_anything_is_read() {
        let temp = TempDir::new().unwrap();
        let mut parts = split_into(temp.path(), b"abcdef", 3);
        fs::remove_file(&parts[1]).unwrap();
        let missing = parts[1].clone();

        match PartsReader::new(&parts) {
            Err(ManagerError::ReadFailed { path, .. }) => assert_eq!(path, missing),
            other => panic!("expected ReadFailed, got {:?}", other.err()),
        }
        assert!(parts.remove(0).exists(), "nothing was consumed");
    }

    #[test]
    fn parts_split_at_awkward_boundaries_extract_like_the_whole_archive() {
        let temp = TempDir::new().unwrap();
        let archive = temp.path().join("whole.tar.gz");
        tar_gz(
            &archive,
            &[
                ("pkg/", b""),
                ("pkg/a.txt", b"alpha"),
                ("pkg/sub/b.txt", b"beta"),
            ],
        );
        let bytes = fs::read(&archive).unwrap();
        // 7 bytes: inside the 10-byte gzip header, then inside tar headers.
        let parts_dir = temp.path().join("parts");
        fs::create_dir_all(&parts_dir).unwrap();
        let parts = split_into(&parts_dir, &bytes, 7);
        let dest = temp.path().join("dest");

        let files = TarGzExtractor::new()
            .extract_parts(&parts, &dest, &CancelToken::new())
            .unwrap();

        assert_eq!(files, 2);
        assert_eq!(fs::read(dest.join("pkg/a.txt")).unwrap(), b"alpha");
        assert_eq!(fs::read(dest.join("pkg/sub/b.txt")).unwrap(), b"beta");
        assert_eq!(
            fs::read_dir(&parts_dir).unwrap().count(),
            0,
            "every part consumed"
        );
    }

    #[test]
    fn extract_parts_refuses_an_entry_outside_the_destination() {
        let temp = TempDir::new().unwrap();
        let archive = temp.path().join("whole.tar.gz");
        tar_gz(
            &archive,
            &[("pkg/ok.txt", b"ok"), ("../escape.txt", b"gotcha")],
        );
        let bytes = fs::read(&archive).unwrap();
        let parts_dir = temp.path().join("parts");
        fs::create_dir_all(&parts_dir).unwrap();
        let parts = split_into(&parts_dir, &bytes, bytes.len() / 2 + 1);
        assert_eq!(parts.len(), 2);
        let dest = temp.path().join("dest");

        let err = TarGzExtractor::new()
            .extract_parts(&parts, &dest, &CancelToken::new())
            .expect_err("an escaping entry must fail the extraction");

        assert!(
            matches!(err, ManagerError::ExtractionFailed { .. }),
            "{err}"
        );
        assert!(err.to_string().contains("outside the destination"), "{err}");
        assert!(!temp.path().join("escape.txt").exists());
    }

    #[test]
    fn a_cancelled_extract_parts_stops_before_the_first_entry() {
        let temp = TempDir::new().unwrap();
        let archive = temp.path().join("whole.tar.gz");
        tar_gz(&archive, &[("pkg/a.txt", b"alpha")]);
        let parts = split_into(temp.path(), &fs::read(&archive).unwrap(), 16);
        let cancel = CancelToken::new();
        cancel.cancel();

        let result =
            TarGzExtractor::new().extract_parts(&parts, &temp.path().join("dest"), &cancel);

        assert!(matches!(result, Err(ManagerError::Cancelled)), "{result:?}");
        assert!(!temp.path().join("dest/pkg/a.txt").exists());
    }

    #[test]
    fn extract_parts_with_no_parts_is_an_extraction_failure() {
        let temp = TempDir::new().unwrap();

        let result = TarGzExtractor::new().extract_parts(
            &[],
            &temp.path().join("dest"),
            &CancelToken::new(),
        );

        assert!(
            matches!(result, Err(ManagerError::ExtractionFailed { .. })),
            "{result:?}"
        );
    }
}
