//! GPU adapter enumeration for the wizard and diagnostics report.
//!
//! Wraps `wgpu::Instance::enumerate_adapters` so callers don't have to
//! depend on wgpu types directly. Also provides the inverse mapping —
//! adapter → `texture.gpu_device` config string — so the wizard's
//! selection can round-trip back through the encoder's `select_adapter`
//! at runtime.
//!
//! # Cost
//!
//! Enumeration takes a noticeable amount of time on multi-adapter
//! systems (observed ~30s on a host with NVIDIA + AMD + GL fallback)
//! because each adapter opens its driver to query info. Callers should
//! treat this as blocking I/O and either do it on a worker thread or
//! show a progress indicator.

use std::fmt;

/// A GPU adapter classification, mirroring `wgpu::DeviceType` but without
/// leaking the wgpu type into our public API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuKind {
    /// Discrete GPU — typically high-performance, dedicated VRAM.
    Discrete,
    /// Integrated GPU — typically lower power, shared with system memory.
    Integrated,
    /// Virtual GPU (e.g., GPU passthrough in a VM).
    Virtual,
    /// Software/CPU rasterizer fallback (llvmpipe, WARP, etc.).
    Cpu,
    /// Could not be classified.
    Other,
}

impl GpuKind {
    /// Human-readable label suitable for wizard UI.
    pub fn display(self) -> &'static str {
        match self {
            GpuKind::Discrete => "Discrete",
            GpuKind::Integrated => "Integrated",
            GpuKind::Virtual => "Virtual",
            GpuKind::Cpu => "Software fallback",
            GpuKind::Other => "Other",
        }
    }
}

/// A single enumerated GPU adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuAdapter {
    /// Adapter name reported by the driver (e.g., "NVIDIA GeForce RTX 4070").
    pub name: String,
    /// Kind/class of the adapter.
    pub kind: GpuKind,
    /// Backend (Vulkan, Metal, DX12, GL, etc.) reported by wgpu, lowercased.
    pub backend: String,
}

impl GpuAdapter {
    /// Compute the `texture.gpu_device` config value that will reselect
    /// this adapter via `dds::compressor::select_adapter` at runtime.
    ///
    /// When the adapter's kind is unique within `all_adapters` and is
    /// either Integrated or Discrete, the kind keyword is preferred
    /// (`"integrated"` or `"discrete"`) — these are stable across driver
    /// updates and easy for users to read in their config file. When the
    /// kind is ambiguous (e.g., two discrete GPUs) or otherwise unhelpful
    /// (Virtual, Cpu, Other), the adapter name is returned so that
    /// `select_adapter`'s case-insensitive substring match picks it.
    pub fn config_value(&self, all_adapters: &[GpuAdapter]) -> String {
        let same_kind_count = all_adapters.iter().filter(|a| a.kind == self.kind).count();
        match self.kind {
            GpuKind::Integrated if same_kind_count == 1 => "integrated".to_string(),
            GpuKind::Discrete if same_kind_count == 1 => "discrete".to_string(),
            _ => self.name.clone(),
        }
    }
}

impl fmt::Display for GpuAdapter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} ({}, {})",
            self.name,
            self.kind.display(),
            self.backend
        )
    }
}

/// Enumerate all GPU adapters visible to wgpu across every backend.
///
/// **This is blocking and slow** (see module docs). Returns an empty
/// vector if no adapters are visible. Errors during driver probing are
/// swallowed at the wgpu layer; the returned list is best-effort.
pub fn enumerate() -> Vec<GpuAdapter> {
    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
        backends: wgpu::Backends::all(),
        ..Default::default()
    });
    let adapters = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::all()));
    adapters
        .into_iter()
        .map(|adapter| {
            let info = adapter.get_info();
            GpuAdapter {
                name: info.name,
                kind: gpu_kind_from(info.device_type),
                backend: format!("{:?}", info.backend).to_lowercase(),
            }
        })
        .collect()
}

fn gpu_kind_from(device_type: wgpu::DeviceType) -> GpuKind {
    match device_type {
        wgpu::DeviceType::DiscreteGpu => GpuKind::Discrete,
        wgpu::DeviceType::IntegratedGpu => GpuKind::Integrated,
        wgpu::DeviceType::VirtualGpu => GpuKind::Virtual,
        wgpu::DeviceType::Cpu => GpuKind::Cpu,
        wgpu::DeviceType::Other => GpuKind::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter(name: &str, kind: GpuKind) -> GpuAdapter {
        GpuAdapter {
            name: name.to_string(),
            kind,
            backend: "vulkan".to_string(),
        }
    }

    #[test]
    fn config_value_uses_kind_keyword_when_unique() {
        let adapters = vec![
            adapter("AMD Radeon Graphics", GpuKind::Integrated),
            adapter("NVIDIA GeForce RTX 4070", GpuKind::Discrete),
        ];
        assert_eq!(adapters[0].config_value(&adapters), "integrated");
        assert_eq!(adapters[1].config_value(&adapters), "discrete");
    }

    #[test]
    fn config_value_falls_back_to_name_when_kind_ambiguous() {
        // Two discrete GPUs: kind keyword can't disambiguate, so the
        // wizard must write the full name to ensure the right one is
        // selected at runtime.
        let adapters = vec![
            adapter("NVIDIA GeForce RTX 4070", GpuKind::Discrete),
            adapter("NVIDIA Quadro P2000", GpuKind::Discrete),
        ];
        assert_eq!(
            adapters[0].config_value(&adapters),
            "NVIDIA GeForce RTX 4070"
        );
        assert_eq!(adapters[1].config_value(&adapters), "NVIDIA Quadro P2000");
    }

    #[test]
    fn config_value_uses_name_for_software_fallback() {
        // Software/Cpu/Virtual/Other adapters are never selectable by
        // kind keyword (the encoder doesn't know those words), so we
        // always emit the name.
        let adapters = vec![adapter("llvmpipe (LLVM 17)", GpuKind::Cpu)];
        assert_eq!(adapters[0].config_value(&adapters), "llvmpipe (LLVM 17)");
    }

    #[test]
    fn display_renders_all_three_fields() {
        let a = adapter("Intel Arc A770", GpuKind::Discrete);
        assert_eq!(a.to_string(), "Intel Arc A770 (Discrete, vulkan)");
    }

    #[test]
    fn gpu_kind_display_strings() {
        assert_eq!(GpuKind::Discrete.display(), "Discrete");
        assert_eq!(GpuKind::Integrated.display(), "Integrated");
        assert_eq!(GpuKind::Virtual.display(), "Virtual");
        assert_eq!(GpuKind::Cpu.display(), "Software fallback");
        assert_eq!(GpuKind::Other.display(), "Other");
    }
}
