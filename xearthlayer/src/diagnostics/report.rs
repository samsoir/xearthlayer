//! System diagnostics report and info collectors.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A comprehensive system diagnostics report.
#[derive(Debug, Clone)]
pub struct SystemReport {
    /// XEarthLayer version
    pub xearthlayer_version: String,
    /// Operating system information
    pub os: OsInfo,
    /// Hardware information
    pub hardware: HardwareInfo,
    /// GPU information
    pub gpu: GpuInfo,
    /// Disk space information
    pub disk: DiskInfo,
    /// Network information
    pub network: NetworkInfo,
    /// XEarthLayer configuration (with secrets redacted)
    pub config: ConfigInfo,
}

/// Operating system information.
#[derive(Debug, Clone, Default)]
pub struct OsInfo {
    pub kernel: Option<String>,
    pub os_name: Option<String>,
    pub desktop: Option<String>,
    pub display_server: Option<String>,
}

/// Hardware information.
#[derive(Debug, Clone, Default)]
pub struct HardwareInfo {
    pub cpu_model: Option<String>,
    pub cpu_cores: Option<u32>,
    pub memory_gb: Option<f64>,
}

/// GPU information.
#[derive(Debug, Clone, Default)]
pub struct GpuInfo {
    pub gpus: Vec<String>,
    pub vram_mb: Option<u64>,
    pub vram_source: Option<String>,
}

/// Disk space information.
///
/// Reports both the root filesystem (annotated when running on an
/// ostree-based atomic distro where 100% rootfs is the steady state)
/// and the writable paths XEarthLayer actually uses (cache directory,
/// package install location). See issue #188 for context.
#[derive(Debug, Clone, Default)]
pub struct DiskInfo {
    /// Root filesystem total size as displayed by `df -h /`.
    pub root_total: Option<String>,
    /// Root filesystem used as displayed by `df -h /`.
    pub root_used: Option<String>,
    /// Root filesystem use% as displayed by `df -h /`.
    pub root_percent: Option<String>,
    /// True if the host appears to be an ostree-based atomic distro
    /// where the rootfs is intentionally sized to its content.
    pub is_immutable_os: bool,
    /// Resolved cache directory path (`cache.directory` from config).
    pub cache_dir_path: Option<String>,
    /// Disk usage of the cache directory (e.g. "12.3G"), via `du -sh`.
    pub cache_dir_size: Option<String>,
    /// Bytes available on the filesystem holding the cache directory.
    pub cache_dir_available: Option<String>,
    /// Resolved package install location (`packages.install_location`
    /// from config, or the default `~/.xearthlayer/packages`).
    pub install_location_path: Option<String>,
    /// Bytes available on the filesystem holding the install location.
    pub install_location_available: Option<String>,
    /// Whether the config directory (`~/.xearthlayer`) exists.
    pub config_exists: bool,
}

/// Network information.
#[derive(Debug, Clone, Default)]
pub struct NetworkInfo {
    pub default_interface: Option<String>,
}

/// XEarthLayer configuration (secrets redacted).
#[derive(Debug, Clone, Default)]
pub struct ConfigInfo {
    pub config_path: Option<String>,
    pub config_contents: Option<String>,
}

impl SystemReport {
    /// Collect system diagnostics.
    pub fn collect() -> Self {
        Self {
            xearthlayer_version: crate::VERSION.to_string(),
            os: OsInfo::collect(),
            hardware: HardwareInfo::collect(),
            gpu: GpuInfo::collect(),
            disk: DiskInfo::collect(),
            network: NetworkInfo::collect(),
            config: ConfigInfo::collect(),
        }
    }
}

impl OsInfo {
    fn collect() -> Self {
        let mut info = Self::default();

        // Kernel version
        if let Ok(output) = Command::new("uname").arg("-r").output() {
            if output.status.success() {
                info.kernel = Some(String::from_utf8_lossy(&output.stdout).trim().to_string());
            }
        }

        // OS Release
        if let Ok(content) = fs::read_to_string("/etc/os-release") {
            for line in content.lines() {
                if line.starts_with("PRETTY_NAME=") {
                    info.os_name = Some(
                        line.trim_start_matches("PRETTY_NAME=")
                            .trim_matches('"')
                            .to_string(),
                    );
                    break;
                }
            }
        }

        // Desktop environment
        if let Ok(desktop) = std::env::var("XDG_CURRENT_DESKTOP") {
            info.desktop = Some(desktop);
        } else if let Ok(desktop) = std::env::var("DESKTOP_SESSION") {
            info.desktop = Some(desktop);
        }

        // Display server
        if let Ok(display) = std::env::var("XDG_SESSION_TYPE") {
            info.display_server = Some(display);
        }

        info
    }
}

impl HardwareInfo {
    fn collect() -> Self {
        let mut info = Self::default();

        // CPU info
        if let Ok(content) = fs::read_to_string("/proc/cpuinfo") {
            let mut cpu_count = 0;

            for line in content.lines() {
                if line.starts_with("model name") && info.cpu_model.is_none() {
                    if let Some(name) = line.split(':').nth(1) {
                        info.cpu_model = Some(name.trim().to_string());
                    }
                }
                if line.starts_with("processor") {
                    cpu_count += 1;
                }
            }

            if cpu_count > 0 {
                info.cpu_cores = Some(cpu_count);
            }
        }

        // Memory info
        if let Ok(content) = fs::read_to_string("/proc/meminfo") {
            for line in content.lines() {
                if line.starts_with("MemTotal:") {
                    if let Some(mem_str) = line.split_whitespace().nth(1) {
                        if let Ok(mem_kb) = mem_str.parse::<u64>() {
                            info.memory_gb = Some(mem_kb as f64 / 1024.0 / 1024.0);
                        }
                    }
                    break;
                }
            }
        }

        info
    }
}

impl GpuInfo {
    fn collect() -> Self {
        let mut info = Self::default();

        // Try lspci for GPU info
        if let Ok(output) = Command::new("lspci").output() {
            if output.status.success() {
                let lspci = String::from_utf8_lossy(&output.stdout);
                for line in lspci.lines() {
                    if line.contains("VGA") || line.contains("3D controller") {
                        if let Some(idx) = line.find(": ") {
                            info.gpus.push(line[idx + 2..].to_string());
                        }
                    }
                }
            }
        }

        // Try NVIDIA VRAM
        if let Ok(output) = Command::new("nvidia-smi")
            .args(["--query-gpu=memory.total", "--format=csv,noheader,nounits"])
            .output()
        {
            if output.status.success() {
                let vram = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if let Ok(mb) = vram.parse::<u64>() {
                    info.vram_mb = Some(mb);
                    info.vram_source = Some("NVIDIA".to_string());
                }
            }
        }

        // Try AMD VRAM
        if info.vram_mb.is_none() {
            if let Ok(paths) = glob::glob("/sys/class/drm/card*/device/mem_info_vram_total") {
                for path in paths.flatten() {
                    if let Ok(content) = fs::read_to_string(&path) {
                        if let Ok(bytes) = content.trim().parse::<u64>() {
                            info.vram_mb = Some(bytes / 1024 / 1024);
                            info.vram_source = Some("AMD".to_string());
                            break;
                        }
                    }
                }
            }
        }

        info
    }
}

impl DiskInfo {
    /// Collect disk info from the user's environment, loading config from
    /// the default path. Falls back to default config if loading fails.
    fn collect() -> Self {
        let config = crate::config::ConfigFile::load().unwrap_or_default();
        Self::from_config(&config)
    }

    /// Collect disk info using the provided config. The cache directory
    /// and package install location are read from the config (with the
    /// install location falling back to `~/.xearthlayer/packages` when
    /// unset, matching the rest of the CLI).
    fn from_config(config: &crate::config::ConfigFile) -> Self {
        let mut info = Self::default();

        // Root filesystem (raw; annotation handled by Display).
        if let Ok(output) = Command::new("df").args(["-h", "/"]).output() {
            if output.status.success() {
                let df = String::from_utf8_lossy(&output.stdout);
                for line in df.lines().skip(1) {
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.len() >= 5 {
                        info.root_total = Some(parts[1].to_string());
                        info.root_used = Some(parts[2].to_string());
                        info.root_percent = Some(parts[4].to_string());
                    }
                }
            }
        }

        info.is_immutable_os = crate::system::is_immutable_os();

        // Cache directory: read from config (already tilde-expanded by parser).
        let cache_dir = &config.cache.directory;
        info.cache_dir_path = Some(cache_dir.display().to_string());
        if cache_dir.exists() {
            if let Ok(output) = Command::new("du")
                .args(["-sh", &cache_dir.to_string_lossy()])
                .output()
            {
                if output.status.success() {
                    let du = String::from_utf8_lossy(&output.stdout);
                    if let Some(size) = du.split_whitespace().next() {
                        info.cache_dir_size = Some(size.to_string());
                    }
                }
            }
        }
        if let Ok(fs) = crate::system::fs_info(cache_dir_for_statvfs(cache_dir)) {
            info.cache_dir_available =
                Some(crate::config::format_size(fs.available_bytes as usize));
        }

        // Package install location: same priority as the CLI
        // (config value > ~/.xearthlayer/packages).
        let install_location = config
            .packages
            .install_location
            .clone()
            .unwrap_or_else(default_install_location);
        info.install_location_path = Some(install_location.display().to_string());
        if let Ok(fs) = crate::system::fs_info(cache_dir_for_statvfs(&install_location)) {
            info.install_location_available =
                Some(crate::config::format_size(fs.available_bytes as usize));
        }

        // Config directory existence (independent of cache/install paths).
        let config_dir = dirs::home_dir().unwrap_or_default().join(".xearthlayer");
        info.config_exists = config_dir.exists();

        info
    }
}

/// Walk up the path until we find an ancestor that exists, so `statvfs`
/// can succeed even when the cache or install directory hasn't been
/// created yet (fresh installs hit this on first diagnostics run).
fn cache_dir_for_statvfs(path: &Path) -> &Path {
    let mut current = path;
    loop {
        if current.exists() {
            return current;
        }
        match current.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => current = parent,
            _ => return Path::new("/"),
        }
    }
}

/// Default package install location when `packages.install_location`
/// is unset, mirroring the priority used by the packages CLI subcommands.
fn default_install_location() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".xearthlayer")
        .join("packages")
}

impl NetworkInfo {
    fn collect() -> Self {
        let mut info = Self::default();

        if let Ok(output) = Command::new("ip")
            .args(["route", "show", "default"])
            .output()
        {
            if output.status.success() {
                let route = String::from_utf8_lossy(&output.stdout);
                if let Some(line) = route.lines().next() {
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if let Some(idx) = parts.iter().position(|&x| x == "dev") {
                        if let Some(iface) = parts.get(idx + 1) {
                            info.default_interface = Some(iface.to_string());
                        }
                    }
                }
            }
        }

        info
    }
}

impl ConfigInfo {
    fn collect() -> Self {
        let mut info = Self::default();

        let home = dirs::home_dir().unwrap_or_default();
        let config_path = home.join(".xearthlayer/config.ini");

        if config_path.exists() {
            info.config_path = Some(config_path.display().to_string());

            if let Ok(content) = fs::read_to_string(&config_path) {
                info.config_contents = Some(redact_sensitive_ini(&content));
            }
        }

        info
    }
}

/// Track the current INI section while walking lines so we can construct
/// `section.key` names and look them up via [`ConfigKey`].
fn parse_section_header(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    if trimmed.starts_with('[') && trimmed.ends_with(']') && trimmed.len() >= 2 {
        Some(&trimmed[1..trimmed.len() - 1])
    } else {
        None
    }
}

/// Redact sensitive credential values from raw INI text by checking each
/// `key = value` line against [`ConfigKey::is_sensitive`] and substituting
/// [`SENSITIVE_VALUE_MASK`] for the value.
///
/// Section headers, comments, blank lines, non-sensitive entries, and
/// entries with empty values are preserved verbatim — empty values aren't
/// secrets and the absence is informative when reading a diagnostics
/// dump. See issue #162 for why we drive both this and `config list`
/// from the same `is_sensitive` predicate.
fn redact_sensitive_ini(content: &str) -> String {
    use crate::config::{ConfigKey, SENSITIVE_VALUE_MASK};

    let mut out = String::with_capacity(content.len());
    let mut current_section: Option<String> = None;

    for line in content.lines() {
        if let Some(section) = parse_section_header(line) {
            current_section = Some(section.to_string());
            out.push_str(line);
            out.push('\n');
            continue;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
            out.push_str(line);
            out.push('\n');
            continue;
        }

        let Some(eq_idx) = line.find('=') else {
            out.push_str(line);
            out.push('\n');
            continue;
        };

        let key_part = line[..eq_idx].trim();
        let value_part = line[eq_idx + 1..].trim();
        let qualified = match &current_section {
            Some(s) => format!("{}.{}", s, key_part),
            None => key_part.to_string(),
        };

        let is_sensitive = qualified
            .parse::<ConfigKey>()
            .map(|k| k.is_sensitive())
            .unwrap_or(false);

        if is_sensitive && !value_part.is_empty() {
            out.push_str(&format!("{} = {}\n", key_part, SENSITIVE_VALUE_MASK));
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }

    out
}

impl fmt::Display for SystemReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "XEarthLayer Diagnostics")?;
        writeln!(f, "=======================")?;
        writeln!(f)?;
        writeln!(f, "XEarthLayer Version: {}", self.xearthlayer_version)?;
        writeln!(f)?;

        // OS
        writeln!(f, "## Operating System")?;
        if let Some(ref kernel) = self.os.kernel {
            writeln!(f, "Kernel: {}", kernel)?;
        }
        if let Some(ref os_name) = self.os.os_name {
            writeln!(f, "OS: {}", os_name)?;
        }
        if let Some(ref desktop) = self.os.desktop {
            writeln!(f, "Desktop: {}", desktop)?;
        }
        if let Some(ref display) = self.os.display_server {
            writeln!(f, "Display Server: {}", display)?;
        }
        writeln!(f)?;

        // Hardware
        writeln!(f, "## Hardware")?;
        if let (Some(ref model), Some(cores)) = (&self.hardware.cpu_model, self.hardware.cpu_cores)
        {
            writeln!(f, "CPU: {} ({} cores)", model, cores)?;
        }
        if let Some(mem) = self.hardware.memory_gb {
            writeln!(f, "Memory: {:.1} GB", mem)?;
        }
        writeln!(f)?;

        // GPU
        writeln!(f, "## GPU")?;
        for gpu in &self.gpu.gpus {
            writeln!(f, "GPU: {}", gpu)?;
        }
        if let (Some(vram), Some(ref source)) = (self.gpu.vram_mb, &self.gpu.vram_source) {
            writeln!(
                f,
                "VRAM ({}): {} MB ({:.1} GB)",
                source,
                vram,
                vram as f64 / 1024.0
            )?;
        }
        writeln!(f)?;

        // GPU Compute Adapters (wgpu) — uses the shared enumerate to
        // keep this surface in lockstep with the wizard's GPU step.
        {
            writeln!(f, "## GPU Compute Adapters")?;
            let adapters = crate::system::enumerate_gpus();
            if adapters.is_empty() {
                writeln!(f, "  (none found)")?;
            } else {
                for (i, adapter) in adapters.iter().enumerate() {
                    writeln!(f, "  [{}] {}", i, adapter)?;
                }
            }
            writeln!(f)?;
        }

        // Disk
        writeln!(f, "## Disk Space")?;

        // Cache directory: writable path XEL actually uses, comes first
        // so users see the relevant signal at the top.
        if let Some(ref path) = self.disk.cache_dir_path {
            let used = self
                .disk
                .cache_dir_size
                .as_deref()
                .unwrap_or("(not created)");
            let avail = self
                .disk
                .cache_dir_available
                .as_deref()
                .unwrap_or("unknown");
            writeln!(
                f,
                "Cache directory: {} ({} used, {} available)",
                path, used, avail
            )?;
        }

        if let Some(ref path) = self.disk.install_location_path {
            let avail = self
                .disk
                .install_location_available
                .as_deref()
                .unwrap_or("unknown");
            writeln!(f, "Packages location: {} ({} available)", path, avail)?;
        }

        // Root filesystem: annotated when the host is an atomic distro
        // because 100% rootfs is the steady state, not a problem.
        if let (Some(ref used), Some(ref total), Some(ref percent)) = (
            &self.disk.root_used,
            &self.disk.root_total,
            &self.disk.root_percent,
        ) {
            if self.disk.is_immutable_os {
                writeln!(
                    f,
                    "Root filesystem: {} used of {} ({}) — normal on atomic distro",
                    used, total, percent
                )?;
            } else {
                writeln!(
                    f,
                    "Root filesystem: {} used of {} ({})",
                    used, total, percent
                )?;
            }
        }

        writeln!(
            f,
            "Config directory: {}",
            if self.disk.config_exists {
                "exists"
            } else {
                "(not created)"
            }
        )?;
        writeln!(f)?;

        // Network
        writeln!(f, "## Network")?;
        if let Some(ref iface) = self.network.default_interface {
            writeln!(f, "Default interface: {}", iface)?;
        }
        writeln!(f)?;

        // Config
        writeln!(f, "## XEarthLayer Configuration")?;
        if let Some(ref path) = self.config.config_path {
            writeln!(f, "Config file: {}", path)?;
            if let Some(ref contents) = self.config.config_contents {
                writeln!(f)?;
                writeln!(f, "```ini")?;
                write!(f, "{}", contents)?;
                writeln!(f, "```")?;
            }
        } else {
            writeln!(f, "Config file: (not created - run 'xearthlayer init')")?;
        }
        writeln!(f)?;

        writeln!(f, "---")?;
        writeln!(f, "Copy the above output into your GitHub issue.")?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConfigFile;
    use tempfile::TempDir;

    /// Build a ConfigFile pointing cache and install location at the
    /// given temp directory subpaths. Used to test that `from_config`
    /// reads from the config rather than hardcoded paths.
    fn config_with_paths(cache_dir: PathBuf, install_location: PathBuf) -> ConfigFile {
        let mut config = ConfigFile::default();
        config.cache.directory = cache_dir;
        config.packages.install_location = Some(install_location);
        config
    }

    #[test]
    fn from_config_reports_cache_directory_from_config() {
        let temp = TempDir::new().unwrap();
        let cache_dir = temp.path().join("xel-cache");
        let install = temp.path().join("xel-packages");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::create_dir_all(&install).unwrap();

        let config = config_with_paths(cache_dir.clone(), install.clone());
        let info = DiskInfo::from_config(&config);

        assert_eq!(
            info.cache_dir_path.as_deref(),
            Some(cache_dir.display().to_string().as_str()),
            "cache_dir_path must reflect config.cache.directory"
        );
        assert_eq!(
            info.install_location_path.as_deref(),
            Some(install.display().to_string().as_str()),
            "install_location_path must reflect config.packages.install_location"
        );
    }

    #[test]
    fn from_config_reports_available_bytes_for_existing_paths() {
        let temp = TempDir::new().unwrap();
        let cache_dir = temp.path().join("cache");
        let install = temp.path().join("packages");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::create_dir_all(&install).unwrap();

        let config = config_with_paths(cache_dir, install);
        let info = DiskInfo::from_config(&config);

        assert!(
            info.cache_dir_available.is_some(),
            "cache_dir_available must be populated when the path exists"
        );
        assert!(
            info.install_location_available.is_some(),
            "install_location_available must be populated when the path exists"
        );
    }

    #[test]
    fn from_config_handles_nonexistent_cache_dir_via_ancestor_walk() {
        // Cache directory doesn't exist yet (fresh install scenario);
        // statvfs should still succeed by walking up to an existing
        // ancestor (the temp dir itself).
        let temp = TempDir::new().unwrap();
        let nonexistent_cache = temp.path().join("not/yet/created/cache");
        let install = temp.path().to_path_buf();

        let config = config_with_paths(nonexistent_cache.clone(), install);
        let info = DiskInfo::from_config(&config);

        assert_eq!(
            info.cache_dir_path.as_deref(),
            Some(nonexistent_cache.display().to_string().as_str()),
            "path is reported as configured even when not yet created"
        );
        assert!(
            info.cache_dir_available.is_some(),
            "available bytes must still be reported via ancestor-walk"
        );
        // The cache dir doesn't exist, so du-based size is None.
        assert!(
            info.cache_dir_size.is_none(),
            "cache_dir_size should be None when path doesn't exist"
        );
    }

    #[test]
    fn from_config_falls_back_to_default_install_location_when_unset() {
        let mut config = ConfigFile::default();
        config.packages.install_location = None;

        let info = DiskInfo::from_config(&config);

        let expected = default_install_location();
        assert_eq!(
            info.install_location_path.as_deref(),
            Some(expected.display().to_string().as_str()),
            "must fall back to ~/.xearthlayer/packages when install_location is unset"
        );
    }

    #[test]
    fn cache_dir_for_statvfs_walks_up_to_existing_ancestor() {
        let temp = TempDir::new().unwrap();
        let nested = temp.path().join("a/b/c/d");
        // None of a/b/c/d exists; ancestor walk should land on temp.path().
        let resolved = cache_dir_for_statvfs(&nested);
        assert!(
            resolved.exists(),
            "resolved path must exist; got {}",
            resolved.display()
        );
    }

    #[test]
    fn redact_sensitive_ini_masks_provider_credentials() {
        let input = "\
[provider]
type = google
google_api_key = AIzaTEST_KEY_NOT_REAL_xxxxxx
mapbox_access_token = pk.eyJ1IjoidGVzdCJ9.fake
";
        let redacted = redact_sensitive_ini(input);
        assert!(redacted.contains("type = google"));
        assert!(redacted.contains("google_api_key = xxxxxxxx"));
        assert!(redacted.contains("mapbox_access_token = xxxxxxxx"));
        assert!(!redacted.contains("AIzaTEST"));
        assert!(!redacted.contains("pk.eyJ1"));
    }

    #[test]
    fn redact_sensitive_ini_preserves_empty_credential_values() {
        // Empty values aren't secrets and the absence is informative
        // when reading a diagnostics dump (e.g., "user hasn't set their
        // Google key yet, that's why google provider isn't working").
        let input = "[provider]\ngoogle_api_key =\n";
        let redacted = redact_sensitive_ini(input);
        assert!(
            redacted.contains("google_api_key ="),
            "empty value must pass through verbatim, got: {:?}",
            redacted
        );
        assert!(!redacted.contains("xxxxxxxx"));
    }

    #[test]
    fn redact_sensitive_ini_preserves_non_credentials_and_structure() {
        let input = "\
# top comment
[cache]
directory = /tmp/cache
memory_size = 512MB

; section comment
[provider]
type = bing
";
        let redacted = redact_sensitive_ini(input);
        // Comments, headers, and non-sensitive values all preserved.
        assert!(redacted.contains("# top comment"));
        assert!(redacted.contains("[cache]"));
        assert!(redacted.contains("directory = /tmp/cache"));
        assert!(redacted.contains("memory_size = 512MB"));
        assert!(redacted.contains("; section comment"));
        assert!(redacted.contains("[provider]"));
        assert!(redacted.contains("type = bing"));
        assert!(!redacted.contains("xxxxxxxx"));
    }

    #[test]
    fn redact_sensitive_ini_passes_through_unknown_keys() {
        // Unknown keys (e.g., from a future XEL version, or user typos)
        // pass through unchanged. The exhaustive `is_sensitive` test in
        // config::keys guarantees we never miss a real sensitive variant.
        let input = "[provider]\nfuture_unknown_key = some_value\n";
        let redacted = redact_sensitive_ini(input);
        assert!(redacted.contains("future_unknown_key = some_value"));
    }

    #[test]
    fn redact_sensitive_ini_only_masks_within_correct_section() {
        // A key literally named `google_api_key` outside `[provider]`
        // would be a parse error in ConfigKey, so passes through. This
        // proves we are NOT falling back to substring matching.
        let input = "\
[cache]
google_api_key = THIS_IS_NOT_A_REAL_KEY_AND_NOT_SENSITIVE
";
        let redacted = redact_sensitive_ini(input);
        assert!(redacted.contains("THIS_IS_NOT_A_REAL_KEY_AND_NOT_SENSITIVE"));
    }
}
