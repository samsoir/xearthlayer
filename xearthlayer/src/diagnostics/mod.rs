//! System diagnostics for bug reports and troubleshooting.
//!
//! This module provides functionality to collect system information
//! that is useful for debugging issues with XEarthLayer.
//!
//! # Example
//!
//! ```ignore
//! use xearthlayer::diagnostics::SystemReport;
//!
//! let report = SystemReport::collect();
//! println!("{}", report);
//! ```

use std::fmt;
use std::fs;
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
#[derive(Debug, Clone, Default)]
pub struct DiskInfo {
    pub root_total: Option<String>,
    pub root_used: Option<String>,
    pub root_percent: Option<String>,
    pub cache_size: Option<String>,
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
    fn collect() -> Self {
        let mut info = Self::default();

        // Root filesystem
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

        // Cache directory
        let home = dirs::home_dir().unwrap_or_default();
        let cache_dir = home.join(".cache/xearthlayer");
        let config_dir = home.join(".xearthlayer");

        if cache_dir.exists() {
            if let Ok(output) = Command::new("du")
                .args(["-sh", &cache_dir.to_string_lossy()])
                .output()
            {
                if output.status.success() {
                    let du = String::from_utf8_lossy(&output.stdout);
                    if let Some(size) = du.split_whitespace().next() {
                        info.cache_size = Some(size.to_string());
                    }
                }
            }
        }

        info.config_exists = config_dir.exists();

        info
    }
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
                let mut redacted = String::new();
                for line in content.lines() {
                    let trimmed = line.trim();
                    if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
                        continue;
                    }
                    if trimmed.to_lowercase().contains("api_key")
                        || trimmed.to_lowercase().contains("secret")
                        || trimmed.to_lowercase().contains("token")
                    {
                        if let Some(key) = trimmed.split('=').next() {
                            redacted.push_str(&format!("{} = [REDACTED]\n", key.trim()));
                        }
                    } else {
                        redacted.push_str(line);
                        redacted.push('\n');
                    }
                }
                info.config_contents = Some(redacted);
            }
        }

        info
    }
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

        // Disk
        writeln!(f, "## Disk Space")?;
        if let (Some(ref used), Some(ref total), Some(ref percent)) = (
            &self.disk.root_used,
            &self.disk.root_total,
            &self.disk.root_percent,
        ) {
            writeln!(
                f,
                "Root filesystem: {} used of {} ({})",
                used, total, percent
            )?;
        }
        if let Some(ref cache) = self.disk.cache_size {
            writeln!(f, "Cache directory: {}", cache)?;
        } else {
            writeln!(f, "Cache directory: (not created)")?;
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
