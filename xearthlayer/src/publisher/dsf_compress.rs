//! In-place 7z compression of DSF files.
//!
//! X-Plane has read a DSF stored as a single-entry 7z archive natively since
//! version 10, and Laminar ships every Global Scenery DSF that way. The
//! profile here matches theirs so a package's DSF files are the shape
//! X-Plane already decodes: LZMA (not LZMA2), a 16 MiB dictionary, one entry
//! named after the file, and a plain header.
//!
//! The dictionary size is the setting that matters for the consumer, not the
//! producer: X-Plane allocates the dictionary per DSF it decodes, so matching
//! Laminar keeps our packages inside the memory budget the sim already has.
//!
//! Nothing else in XEarthLayer needs to know. FUSE serves the file by name
//! and never parses it, and the archive container around the package is
//! unchanged.

use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::Path;

use sevenz_rust2::encoder_options::LzmaOptions;
use sevenz_rust2::{ArchiveEntry, ArchiveWriter};

use super::{PublishError, PublishResult};

/// Signature bytes at the start of every 7z archive.
pub const SEVENZ_MAGIC: [u8; 6] = [0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C];

/// Dictionary size Laminar uses for Global Scenery DSF files (`7z l` reports
/// `LZMA:24`, that is 2^24 bytes).
pub const DSF_DICT_SIZE: u32 = 1 << 24;

/// LZMA preset. 7 is the xz preset whose dictionary is already 16 MiB, so the
/// explicit dictionary size only pins what the preset chooses.
pub const DSF_LZMA_LEVEL: u32 = 7;

/// Returns true if `bytes` begins with the 7z signature.
pub fn is_sevenz(bytes: &[u8]) -> bool {
    bytes.len() >= SEVENZ_MAGIC.len() && bytes[..SEVENZ_MAGIC.len()] == SEVENZ_MAGIC
}

/// Returns true if the file at `path` begins with the 7z signature.
///
/// Reads at most six bytes; a file shorter than the signature is not 7z.
pub fn is_sevenz_file(path: &Path) -> io::Result<bool> {
    let mut head = [0u8; SEVENZ_MAGIC.len()];
    let mut file = File::open(path)?;
    let mut filled = 0;
    while filled < head.len() {
        let n = file.read(&mut head[filled..])?;
        if n == 0 {
            return Ok(false);
        }
        filled += n;
    }
    Ok(is_sevenz(&head))
}

/// Byte accounting for one compressed DSF.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DsfCompressStats {
    /// Size of the source file.
    pub raw_bytes: u64,
    /// Size of the file written to the package.
    pub stored_bytes: u64,
    /// The source was already a 7z container and was copied unchanged.
    pub already_compressed: bool,
}

/// Writes a DSF as a single-entry 7z container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DsfCompressor {
    level: u32,
    dict_size: u32,
}

impl DsfCompressor {
    /// The profile Laminar's own Global Scenery DSF files use.
    pub fn laminar() -> Self {
        Self {
            level: DSF_LZMA_LEVEL,
            dict_size: DSF_DICT_SIZE,
        }
    }

    /// Compress `src` into `dest`.
    ///
    /// The archive holds one entry named after `src`'s file name, which is
    /// what X-Plane expects. A source that is already a 7z container is
    /// copied as-is so a second pass over a package is a no-op.
    pub fn compress_file(&self, src: &Path, dest: &Path) -> PublishResult<DsfCompressStats> {
        let read_err = |e: io::Error| PublishError::ReadFailed {
            path: src.to_path_buf(),
            source: e,
        };
        let write_err = |e: io::Error| PublishError::WriteFailed {
            path: dest.to_path_buf(),
            source: e,
        };

        let raw_bytes = fs::metadata(src).map_err(read_err)?.len();

        if is_sevenz_file(src).map_err(read_err)? {
            fs::copy(src, dest).map_err(write_err)?;
            return Ok(DsfCompressStats {
                raw_bytes,
                stored_bytes: raw_bytes,
                already_compressed: true,
            });
        }

        let entry_name = src.file_name().and_then(|n| n.to_str()).ok_or_else(|| {
            PublishError::InvalidPath(format!("{} has no file name", src.display()))
        })?;

        let input = BufReader::new(File::open(src).map_err(read_err)?);
        let output = File::create(dest).map_err(write_err)?;

        let compress_err = |e: sevenz_rust2::Error| PublishError::DsfCompressionFailed {
            path: src.to_path_buf(),
            message: e.to_string(),
        };

        let mut writer = ArchiveWriter::new(BufWriter::new(output)).map_err(compress_err)?;
        let mut options = LzmaOptions::from_level(self.level);
        options.set_dictionary_size(self.dict_size);
        writer.set_content_methods(vec![options.into()]);
        writer
            .push_archive_entry(ArchiveEntry::new_file(entry_name), Some(input))
            .map_err(compress_err)?;
        // `finish` hands the sink back; flush it here rather than trust
        // `BufWriter`'s drop, which swallows errors.
        let mut sink = writer.finish().map_err(write_err)?;
        sink.flush().map_err(write_err)?;

        let stored_bytes = fs::metadata(dest)
            .map_err(|e| PublishError::ReadFailed {
                path: dest.to_path_buf(),
                source: e,
            })?
            .len();

        Ok(DsfCompressStats {
            raw_bytes,
            stored_bytes,
            already_compressed: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// 96 KiB of low-entropy but non-constant bytes, so LZMA has something to
    /// find and the round trip is a real test rather than an all-zero one.
    fn sample_dsf_bytes() -> Vec<u8> {
        (0..96 * 1024u32)
            .map(|i| ((i / 7) % 251) as u8 ^ (i % 13) as u8)
            .collect()
    }

    /// Parse the 7z start header and return the first byte of the next
    /// header: 0x01 is `kHeader` (plain), 0x17 is `kEncodedHeader`.
    fn next_header_kind(archive: &[u8]) -> u8 {
        let next_off = u64::from_le_bytes(archive[12..20].try_into().unwrap()) as usize;
        archive[32 + next_off]
    }

    #[test]
    fn magic_matches_7z_signature() {
        assert_eq!(SEVENZ_MAGIC, [0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C]);
        assert!(is_sevenz(&SEVENZ_MAGIC));
        assert!(!is_sevenz(b"XPLNEDSF"));
        assert!(!is_sevenz(&SEVENZ_MAGIC[..5]));
    }

    #[test]
    fn laminar_profile_pins_dictionary_and_level() {
        let c = DsfCompressor::laminar();
        assert_eq!(
            c.dict_size,
            1 << 24,
            "Laminar Global Scenery DSF report LZMA:24"
        );
        assert_eq!(c.level, DSF_LZMA_LEVEL);
    }

    #[test]
    fn compress_file_round_trips_and_names_entry_after_file() {
        let temp = TempDir::new().unwrap();
        let src = temp.path().join("+40-074.dsf");
        let dest = temp.path().join("out").join("+40-074.dsf");
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        let raw = sample_dsf_bytes();
        fs::write(&src, &raw).unwrap();

        let stats = DsfCompressor::laminar().compress_file(&src, &dest).unwrap();

        assert_eq!(stats.raw_bytes, raw.len() as u64);
        assert!(!stats.already_compressed);
        let stored = fs::read(&dest).unwrap();
        assert_eq!(stats.stored_bytes, stored.len() as u64);
        assert!(is_sevenz(&stored));
        assert!(stored.len() < raw.len(), "LZMA must shrink the sample");

        let mut reader =
            sevenz_rust2::ArchiveReader::open(&dest, sevenz_rust2::Password::empty()).unwrap();
        let names: Vec<String> = reader
            .archive()
            .files
            .iter()
            .map(|e| e.name().to_string())
            .collect();
        assert_eq!(names, vec!["+40-074.dsf".to_string()]);
        assert_eq!(reader.read_file("+40-074.dsf").unwrap(), raw);
    }

    #[test]
    fn compress_file_writes_plain_header_like_laminar() {
        let temp = TempDir::new().unwrap();
        let src = temp.path().join("+40-074.dsf");
        let dest = temp.path().join("+40-074.7z");
        fs::write(&src, sample_dsf_bytes()).unwrap();

        DsfCompressor::laminar().compress_file(&src, &dest).unwrap();

        let stored = fs::read(&dest).unwrap();
        assert_eq!(
            next_header_kind(&stored),
            0x01,
            "X-Plane's reference files use kHeader"
        );
    }

    #[test]
    fn compress_file_copies_already_compressed_input_unchanged() {
        let temp = TempDir::new().unwrap();
        let raw = temp.path().join("raw.dsf");
        let once = temp.path().join("once.dsf");
        let twice = temp.path().join("twice.dsf");
        fs::write(&raw, sample_dsf_bytes()).unwrap();
        DsfCompressor::laminar().compress_file(&raw, &once).unwrap();

        let stats = DsfCompressor::laminar()
            .compress_file(&once, &twice)
            .unwrap();

        assert!(stats.already_compressed);
        assert_eq!(stats.raw_bytes, stats.stored_bytes);
        assert_eq!(fs::read(&once).unwrap(), fs::read(&twice).unwrap());
    }

    #[test]
    fn compress_file_reports_missing_source() {
        let temp = TempDir::new().unwrap();
        let err = DsfCompressor::laminar()
            .compress_file(&temp.path().join("nope.dsf"), &temp.path().join("out.dsf"))
            .unwrap_err();
        assert!(
            matches!(err, PublishError::ReadFailed { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn is_sevenz_file_reads_only_the_signature() {
        let temp = TempDir::new().unwrap();
        let short = temp.path().join("short");
        fs::write(&short, b"XP").unwrap();
        assert!(!is_sevenz_file(&short).unwrap());
        let sig = temp.path().join("sig");
        fs::write(&sig, SEVENZ_MAGIC).unwrap();
        assert!(is_sevenz_file(&sig).unwrap());
    }
}
