//! Archive building for package distribution.
//!
//! Archives a package directory as `tar.gz` and cuts it into parts for
//! distribution, entirely in process: no `tar` or `split` binary is needed
//! (#274). The archive streams straight into its parts, each hashed as it is
//! written, so the package is read once and nothing is written twice.
//!
//! The output stays what system `tar -czf` piped through `split -b` would
//! produce in form: one gzip stream holding the package directory, cut into
//! parts named `.aa`, `.ab` and so on at exactly `part_size` bytes. Entries
//! are written in sorted order and the gzip header carries no timestamp, so
//! the same tree always yields the same parts and checksums.

use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

use flate2::write::GzEncoder;
use flate2::Compression;
use sha2::{Digest, Sha256};

use semver::Version;

use super::{PublishError, PublishResult, RepoConfig};
use xearthlayer_package::{self as package, PackageType};

// Re-export format_size for convenience
pub use crate::size::format_size as format_archive_size;

/// Result of building an archive.
#[derive(Debug, Clone)]
pub struct ArchiveBuildResult {
    /// The base archive filename (e.g., "zzXEL_na_ortho-1.0.0.tar.gz").
    pub archive_name: String,

    /// List of archive parts with their paths and checksums.
    pub parts: Vec<ArchivePart>,

    /// Total size of all parts in bytes.
    pub total_size: u64,
}

/// Information about a single archive part.
#[derive(Debug, Clone)]
pub struct ArchivePart {
    /// Filename of this part (e.g., "zzXEL_na_ortho-1.0.0.tar.gz.aa").
    pub filename: String,

    /// Full path to the part file.
    pub path: PathBuf,

    /// SHA-256 checksum of this part.
    pub checksum: String,

    /// Size of this part in bytes.
    pub size: u64,
}

/// Generate the archive filename for a package.
///
/// This is a re-export of [`xearthlayer_package::archive_filename`] for convenience.
/// See that function for full documentation.
pub fn archive_filename(region: &str, package_type: PackageType, version: &Version) -> String {
    package::archive_filename(region, package_type, version)
}

/// Build a distributable archive from a package directory.
///
/// # Arguments
///
/// * `package_dir` - Path to the package directory to archive
/// * `dist_dir` - Directory to write archive parts to
/// * `region` - Region code for the package
/// * `package_type` - Type of package (ortho/overlay)
/// * `version` - Package version
/// * `config` - Repository configuration (for part size)
///
/// # Returns
///
/// Information about the built archive and its parts.
pub fn build_archive(
    package_dir: &Path,
    dist_dir: &Path,
    region: &str,
    package_type: PackageType,
    version: &Version,
    config: &RepoConfig,
) -> PublishResult<ArchiveBuildResult> {
    // Validate package directory exists
    if !package_dir.exists() || !package_dir.is_dir() {
        return Err(PublishError::InvalidPath(format!(
            "package directory does not exist: {}",
            package_dir.display()
        )));
    }

    // Create dist subdirectory for this package type
    // The region is joined straight into the dist path, so it is untrusted
    // input to a path until it has been checked.
    xearthlayer_package::validate_region(region)?;

    let type_dist_dir = dist_dir
        .join(region.to_lowercase())
        .join(package_type.folder_suffix());
    fs::create_dir_all(&type_dist_dir).map_err(|e| PublishError::CreateDirectoryFailed {
        path: type_dist_dir.clone(),
        source: e,
    })?;

    let archive_name = archive_filename(region, package_type, version);
    let parts = write_split_archive(package_dir, &type_dist_dir, &archive_name, config.part_size)?;

    let total_size = parts.iter().map(|p| p.size).sum();

    Ok(ArchiveBuildResult {
        archive_name,
        parts,
        total_size,
    })
}

/// Archive `source_dir` as `tar.gz`, streamed straight into parts of
/// `part_size` bytes in `out_dir`. The archive's single top-level entry is
/// the directory itself, as `tar -czf <archive> <dir>` run from its parent.
fn write_split_archive(
    source_dir: &Path,
    out_dir: &Path,
    archive_name: &str,
    part_size: u64,
) -> PublishResult<Vec<ArchivePart>> {
    let dir_name = source_dir
        .file_name()
        .ok_or_else(|| PublishError::InvalidPath("invalid source directory name".to_string()))?;

    let parts = PartWriter::new(out_dir, archive_name, part_size);
    let mut tar = tar::Builder::new(GzEncoder::new(parts, Compression::default()));
    // Store links as links, as `tar` does, rather than archiving their targets.
    tar.follow_symlinks(false);
    append_tree(&mut tar, source_dir, Path::new(dir_name))?;

    let failed = |e: io::Error| PublishError::ArchiveFailed(format!("{}: {}", archive_name, e));
    let gz = tar.into_inner().map_err(failed)?;
    let parts = gz.finish().map_err(failed)?;
    parts.finish()
}

/// Append `dir` and everything beneath it to `tar` as `name`, in sorted
/// order so the same tree always produces the same archive.
fn append_tree<W: Write>(tar: &mut tar::Builder<W>, dir: &Path, name: &Path) -> PublishResult<()> {
    let failed = |path: &Path, e: io::Error| {
        PublishError::ArchiveFailed(format!("{}: {}", path.display(), e))
    };

    tar.append_dir(name, dir).map_err(|e| failed(dir, e))?;
    let mut entries = fs::read_dir(dir)
        .and_then(|entries| entries.collect::<io::Result<Vec<_>>>())
        .map_err(|e| PublishError::ReadFailed {
            path: dir.to_path_buf(),
            source: e,
        })?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let path = entry.path();
        let entry_name = name.join(entry.file_name());
        // `file_type` does not follow symlinks, so a link to a directory is
        // archived as a link, not descended into.
        let is_dir = entry.file_type().map_err(|e| failed(&path, e))?.is_dir();
        if is_dir {
            append_tree(tar, &path, &entry_name)?;
        } else {
            tar.append_path_with_name(&path, &entry_name)
                .map_err(|e| failed(&path, e))?;
        }
    }
    Ok(())
}

/// Letters in a part suffix, as `split`'s default `-a 2`.
const SUFFIX_LETTERS: u32 = 2;

/// The suffix `split -b` gives part `index`: `aa`, `ab`, ... `zz`.
fn part_suffix(index: usize) -> Option<String> {
    if index >= 26usize.pow(SUFFIX_LETTERS) {
        return None;
    }
    let letter = |n: usize| char::from(b'a' + n as u8);
    Some([letter(index / 26), letter(index % 26)].iter().collect())
}

/// A part file being written, with the running hash of its contents.
struct OpenPart {
    file: BufWriter<File>,
    hasher: Sha256,
    written: u64,
    filename: String,
    path: PathBuf,
}

/// Cuts a byte stream into part files of exactly `part_size` bytes, named
/// and sized as `split -b` would, hashing each part as it is written so no
/// part has to be read back.
struct PartWriter {
    dir: PathBuf,
    archive_name: String,
    part_size: u64,
    current: Option<OpenPart>,
    parts: Vec<ArchivePart>,
}

impl PartWriter {
    fn new(dir: &Path, archive_name: &str, part_size: u64) -> Self {
        Self {
            dir: dir.to_path_buf(),
            archive_name: archive_name.to_string(),
            part_size,
            current: None,
            parts: Vec::new(),
        }
    }

    /// Flush and record the part being written, if any.
    fn close_current(&mut self) -> io::Result<()> {
        if let Some(mut part) = self.current.take() {
            part.file.flush().map_err(|e| with_path(&part.path, e))?;
            self.parts.push(ArchivePart {
                filename: part.filename,
                path: part.path,
                checksum: format!("{:x}", part.hasher.finalize()),
                size: part.written,
            });
        }
        Ok(())
    }

    fn open_next(&mut self) -> io::Result<&mut OpenPart> {
        let suffix = part_suffix(self.parts.len()).ok_or_else(|| {
            io::Error::other(format!(
                "{} needs more than {} parts; raise part_size",
                self.archive_name,
                26usize.pow(SUFFIX_LETTERS)
            ))
        })?;
        let filename = format!("{}.{}", self.archive_name, suffix);
        let path = self.dir.join(&filename);
        let file = File::create(&path).map_err(|e| with_path(&path, e))?;
        Ok(self.current.insert(OpenPart {
            file: BufWriter::new(file),
            hasher: Sha256::new(),
            written: 0,
            filename,
            path,
        }))
    }

    fn finish(mut self) -> PublishResult<Vec<ArchivePart>> {
        self.close_current()
            .map_err(|e| PublishError::ArchiveFailed(e.to_string()))?;
        if self.parts.is_empty() {
            return Err(PublishError::ArchiveFailed(format!(
                "{} produced no data",
                self.archive_name
            )));
        }
        Ok(self.parts)
    }
}

impl Write for PartWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        // Parts open on demand, so a stream that ends exactly on a boundary
        // leaves no empty trailing part, as with `split`.
        if self
            .current
            .as_ref()
            .is_some_and(|part| part.written == self.part_size)
        {
            self.close_current()?;
        }
        let part_size = self.part_size;
        let part = match self.current {
            Some(ref mut part) => part,
            None => self.open_next()?,
        };
        let room = (part_size - part.written).min(buf.len() as u64) as usize;
        part.file
            .write_all(&buf[..room])
            .map_err(|e| with_path(&part.path, e))?;
        part.hasher.update(&buf[..room]);
        part.written += room as u64;
        Ok(room)
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.current {
            Some(ref mut part) => part.file.flush().map_err(|e| with_path(&part.path, e)),
            None => Ok(()),
        }
    }
}

/// Prefix an I/O error with the file it concerns.
fn with_path(path: &Path, e: io::Error) -> io::Error {
    io::Error::new(e.kind(), format!("{}: {}", path.display(), e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_archive_filename_ortho() {
        let version = Version::new(1, 2, 3);
        let filename = archive_filename("na", PackageType::Ortho, &version);
        assert_eq!(filename, "zzXEL_na_ortho-1.2.3.tar.gz");
    }

    #[test]
    fn test_archive_filename_overlay() {
        let version = Version::new(2, 0, 0);
        let filename = archive_filename("EUR", PackageType::Overlay, &version);
        assert_eq!(filename, "yzXEL_eur_overlay-2.0.0.tar.gz");
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        format!("{:x}", Sha256::digest(bytes))
    }

    #[test]
    fn part_suffixes_follow_split_naming() {
        assert_eq!(part_suffix(0).as_deref(), Some("aa"));
        assert_eq!(part_suffix(1).as_deref(), Some("ab"));
        assert_eq!(part_suffix(25).as_deref(), Some("az"));
        assert_eq!(part_suffix(26).as_deref(), Some("ba"));
        assert_eq!(part_suffix(675).as_deref(), Some("zz"));
        // split -a 2 runs out here too.
        assert_eq!(part_suffix(676), None);
    }

    #[test]
    fn a_stream_is_cut_into_parts_of_exactly_the_part_size() {
        let temp = TempDir::new().unwrap();
        let mut writer = PartWriter::new(temp.path(), "pkg.tar.gz", 4);
        // Writes that straddle part boundaries.
        for chunk in [&b"abc"[..], b"defgh", b"ij"] {
            writer.write_all(chunk).unwrap();
        }
        let parts = writer.finish().unwrap();

        let expected: [(&str, &[u8]); 3] = [
            ("pkg.tar.gz.aa", b"abcd"),
            ("pkg.tar.gz.ab", b"efgh"),
            ("pkg.tar.gz.ac", b"ij"),
        ];
        assert_eq!(parts.len(), expected.len());
        for (part, (name, bytes)) in parts.iter().zip(expected) {
            assert_eq!(part.filename, name);
            assert_eq!(part.path, temp.path().join(name));
            assert_eq!(fs::read(&part.path).unwrap(), bytes);
            assert_eq!(part.size, bytes.len() as u64);
            assert_eq!(part.checksum, sha256_hex(bytes));
        }
    }

    #[test]
    fn a_stream_that_fills_its_last_part_exactly_leaves_no_empty_part() {
        let temp = TempDir::new().unwrap();
        let mut writer = PartWriter::new(temp.path(), "pkg.tar.gz", 4);
        writer.write_all(b"abcdefgh").unwrap();

        let parts = writer.finish().unwrap();

        assert_eq!(parts.len(), 2);
        assert!(!temp.path().join("pkg.tar.gz.ac").exists());
    }

    /// A package tree with nested directories, a name with spaces (as in
    /// `Earth nav data`) and binary content.
    fn write_package_tree(root: &Path) -> PathBuf {
        let pkg = root.join("zzXEL_na_ortho");
        let files: [(&str, &[u8]); 3] = [
            ("Earth nav data/+40-080/+45-075.dsf", b"DSF bytes"),
            ("terrain/12345_67890_ZL16.ter", b"A\n800\nTERRAIN\n"),
            (
                "textures/12345_67890_ZL16.dds",
                &[0x44, 0x44, 0x53, 0x20, 0, 255],
            ),
        ];
        for (name, data) in files {
            let path = pkg.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, data).unwrap();
        }
        pkg
    }

    fn build(package_dir: &Path, dist_dir: &Path) -> ArchiveBuildResult {
        build_archive(
            package_dir,
            dist_dir,
            "na",
            PackageType::Ortho,
            &Version::new(1, 0, 0),
            &RepoConfig::default(),
        )
        .unwrap()
    }

    #[test]
    fn the_archive_unpacks_to_the_package_directory_and_everything_in_it() {
        let temp = TempDir::new().unwrap();
        let pkg = write_package_tree(&temp.path().join("src"));
        let result = build(&pkg, &temp.path().join("dist"));

        let mut archive_bytes = Vec::new();
        for part in &result.parts {
            archive_bytes.extend(fs::read(&part.path).unwrap());
        }
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(&archive_bytes[..]));
        let mut files = std::collections::BTreeMap::new();
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            if entry.header().entry_type().is_file() {
                let name = entry.path().unwrap().to_string_lossy().into_owned();
                let mut data = Vec::new();
                std::io::Read::read_to_end(&mut entry, &mut data).unwrap();
                files.insert(name, data);
            }
        }

        assert_eq!(
            files.keys().cloned().collect::<Vec<_>>(),
            [
                "zzXEL_na_ortho/Earth nav data/+40-080/+45-075.dsf",
                "zzXEL_na_ortho/terrain/12345_67890_ZL16.ter",
                "zzXEL_na_ortho/textures/12345_67890_ZL16.dds",
            ]
        );
        assert_eq!(
            files["zzXEL_na_ortho/textures/12345_67890_ZL16.dds"],
            [0x44, 0x44, 0x53, 0x20, 0, 255]
        );
    }

    #[test]
    fn building_the_same_tree_twice_gives_the_same_checksums() {
        let temp = TempDir::new().unwrap();
        let pkg = write_package_tree(&temp.path().join("src"));

        let first = build(&pkg, &temp.path().join("dist1"));
        let second = build(&pkg, &temp.path().join("dist2"));

        let checksums = |r: &ArchiveBuildResult| {
            r.parts
                .iter()
                .map(|p| p.checksum.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(checksums(&first), checksums(&second));
    }

    #[test]
    fn test_build_archive_small_file() {
        let temp = TempDir::new().unwrap();

        // Create a package directory with some content
        let package_dir = temp.path().join("zzXEL_na_ortho");
        fs::create_dir_all(&package_dir).unwrap();

        // Create some test files
        let metadata_path = package_dir.join("xearthlayer_scenery_package.txt");
        File::create(&metadata_path)
            .unwrap()
            .write_all(b"test metadata content")
            .unwrap();

        let terrain_dir = package_dir.join("terrain");
        fs::create_dir_all(&terrain_dir).unwrap();
        File::create(terrain_dir.join("test.ter"))
            .unwrap()
            .write_all(b"test terrain file")
            .unwrap();

        // Create dist directory
        let dist_dir = temp.path().join("dist");
        fs::create_dir_all(&dist_dir).unwrap();

        // Build archive with large part size (won't split)
        let config = RepoConfig::new(500 * 1024 * 1024).unwrap();
        let version = Version::new(1, 0, 0);

        let result = build_archive(
            &package_dir,
            &dist_dir,
            "na",
            PackageType::Ortho,
            &version,
            &config,
        )
        .unwrap();

        assert_eq!(result.archive_name, "zzXEL_na_ortho-1.0.0.tar.gz");
        assert_eq!(result.parts.len(), 1);
        assert!(result.parts[0].filename.ends_with(".aa"));
        assert!(result.parts[0].path.exists());
        assert!(!result.parts[0].checksum.is_empty());
        assert!(result.total_size > 0);
    }

    #[test]
    fn test_build_archive_splits_large_file() {
        use rand::Rng;

        let temp = TempDir::new().unwrap();

        // Create a package directory with larger content
        let package_dir = temp.path().join("zzXEL_na_ortho");
        fs::create_dir_all(&package_dir).unwrap();

        // Create a file larger than the part size with random data (incompressible)
        let data_path = package_dir.join("large_data.bin");
        let mut file = File::create(&data_path).unwrap();
        let mut rng = rand::rng();

        // Write 25 MB of random data (incompressible, will be split with 10 MB parts)
        let mut chunk = vec![0u8; 1024 * 1024]; // 1 MB chunk
        for _ in 0..25 {
            rng.fill(&mut chunk[..]);
            file.write_all(&chunk).unwrap();
        }
        drop(file);

        let dist_dir = temp.path().join("dist");
        fs::create_dir_all(&dist_dir).unwrap();

        // Build with 10 MB part size (minimum allowed)
        let config = RepoConfig::new(10 * 1024 * 1024).unwrap();
        let version = Version::new(1, 0, 0);

        let result = build_archive(
            &package_dir,
            &dist_dir,
            "na",
            PackageType::Ortho,
            &version,
            &config,
        )
        .unwrap();

        // Should have multiple parts
        assert!(
            result.parts.len() > 1,
            "expected multiple parts, got {}",
            result.parts.len()
        );

        // Parts should be sorted
        let suffixes: Vec<&str> = result
            .parts
            .iter()
            .map(|p| p.filename.rsplit('.').next().unwrap())
            .collect();
        assert!(suffixes.windows(2).all(|w| w[0] <= w[1]));

        // All parts should exist with checksums
        for part in &result.parts {
            assert!(part.path.exists());
            assert!(!part.checksum.is_empty());
            assert_eq!(part.checksum.len(), 64); // SHA-256 hex
        }
    }

    #[test]
    fn test_build_archive_creates_dist_subdirs() {
        let temp = TempDir::new().unwrap();

        let package_dir = temp.path().join("zzXEL_eur_ortho");
        fs::create_dir_all(&package_dir).unwrap();
        File::create(package_dir.join("test.txt"))
            .unwrap()
            .write_all(b"test")
            .unwrap();

        let dist_dir = temp.path().join("dist");
        // Don't create dist_dir - build_archive should create it

        let config = RepoConfig::default();
        let version = Version::new(1, 0, 0);

        let result = build_archive(
            &package_dir,
            &dist_dir,
            "eur",
            PackageType::Ortho,
            &version,
            &config,
        )
        .unwrap();

        // Should have created dist/eur/ortho/
        assert!(dist_dir.join("eur").join("ortho").exists());
        assert!(result.parts[0].path.exists());
    }

    #[test]
    fn test_build_archive_invalid_package_dir() {
        let temp = TempDir::new().unwrap();
        let dist_dir = temp.path().join("dist");

        let config = RepoConfig::default();
        let version = Version::new(1, 0, 0);

        let result = build_archive(
            Path::new("/nonexistent/path"),
            &dist_dir,
            "na",
            PackageType::Ortho,
            &version,
            &config,
        );

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("does not exist"));
    }

    #[test]
    fn test_archive_part_debug() {
        let part = ArchivePart {
            filename: "test.tar.gz.aa".to_string(),
            path: PathBuf::from("/tmp/test"),
            checksum: "abc123".to_string(),
            size: 1024,
        };
        let debug = format!("{:?}", part);
        assert!(debug.contains("ArchivePart"));
        assert!(debug.contains("test.tar.gz.aa"));
    }

    #[test]
    fn test_archive_build_result_debug() {
        let result = ArchiveBuildResult {
            archive_name: "test.tar.gz".to_string(),
            parts: vec![],
            total_size: 0,
        };
        let debug = format!("{:?}", result);
        assert!(debug.contains("ArchiveBuildResult"));
    }

    #[test]
    fn building_an_archive_rejects_a_region_that_could_escape_the_dist_directory() {
        let temp = TempDir::new().unwrap();

        let package_dir = temp.path().join("zzXEL_na_ortho");
        fs::create_dir_all(&package_dir).unwrap();
        File::create(package_dir.join("payload.bin"))
            .unwrap()
            .write_all(b"payload")
            .unwrap();

        let dist_dir = temp.path().join("dist");
        fs::create_dir_all(&dist_dir).unwrap();

        // Resolves to temp/victim, outside dist.
        let escaping = "../victim";
        let victim = temp.path().join("victim");

        let config = RepoConfig::new(500 * 1024 * 1024).unwrap();
        let err = build_archive(
            &package_dir,
            &dist_dir,
            escaping,
            PackageType::Ortho,
            &Version::new(1, 0, 0),
            &config,
        )
        .expect_err("a region that escapes the dist directory must be refused");

        assert!(
            err.to_string().contains(escaping),
            "the error should name the offending region: {err}"
        );
        assert!(
            !victim.exists(),
            "nothing may be created outside the dist directory"
        );
    }
}
