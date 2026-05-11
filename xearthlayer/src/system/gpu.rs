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
use thiserror::Error;

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
    /// Build a metadata-only `GpuAdapter` from a live `wgpu::Adapter`.
    pub fn from_wgpu(adapter: &wgpu::Adapter) -> Self {
        let info = adapter.get_info();
        Self {
            name: info.name,
            kind: gpu_kind_from(info.device_type),
            backend: format!("{:?}", info.backend).to_lowercase(),
        }
    }

    /// Compute the `texture.gpu_device` config value that will reselect
    /// this adapter via [`find_adapter`] at runtime.
    ///
    /// When the adapter's kind is unique within `all_adapters` and is
    /// either Integrated or Discrete, the kind keyword is preferred
    /// (`"integrated"` or `"discrete"`) — these are stable across driver
    /// updates and easy for users to read in their config file. When the
    /// kind is ambiguous (e.g., two discrete GPUs) or otherwise unhelpful
    /// (Virtual, Cpu, Other), the adapter name is returned so that
    /// the case-insensitive substring match in [`select_index`] picks it.
    ///
    /// The pairing of [`config_value`](Self::config_value) and
    /// [`select_index`] is enforced by `config_value_and_select_index_round_trip`
    /// — whenever you touch one, run the test to confirm the other still
    /// finds what was written.
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

/// Errors from [`find_adapter`] and related selection helpers.
#[derive(Debug, Error)]
pub enum GpuSelectError {
    /// No GPU adapters were visible at all (driver issue, headless host).
    #[error("No GPU adapters available")]
    NoneAvailable,
    /// At least one adapter was found, but none matched the selector.
    #[error("No GPU adapter matching '{gpu_device}'. Available adapters:\n{available}")]
    NoMatch {
        gpu_device: String,
        available: String,
    },
}

/// Enumerate all GPU adapters as live `wgpu::Adapter` handles.
///
/// Most callers should prefer [`enumerate`] which returns metadata-only
/// `GpuAdapter` records. Use this when you need the live adapter to
/// call `request_device` on (e.g., the DDS GPU compressor).
///
/// **This is blocking and slow** (see module docs). Returns an empty
/// vector if no adapters are visible.
pub fn enumerate_raw() -> Vec<wgpu::Adapter> {
    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
        backends: wgpu::Backends::all(),
        ..Default::default()
    });
    pollster::block_on(instance.enumerate_adapters(wgpu::Backends::all()))
}

/// Enumerate all GPU adapters as metadata-only [`GpuAdapter`] records.
pub fn enumerate() -> Vec<GpuAdapter> {
    enumerate_raw().iter().map(GpuAdapter::from_wgpu).collect()
}

/// Find a live `wgpu::Adapter` matching the given `gpu_device` selector.
///
/// Selector semantics:
/// - `"integrated"` or `"discrete"` (case-insensitive): match by [`GpuKind`]
/// - anything else: case-insensitive substring match against adapter name
///
/// This is the inverse of [`GpuAdapter::config_value`] — they round-trip
/// for any adapter present in `adapters` (verified by the round-trip
/// test in this module).
pub fn find_adapter<'a>(
    adapters: &'a [wgpu::Adapter],
    gpu_device: &str,
) -> Result<&'a wgpu::Adapter, GpuSelectError> {
    if adapters.is_empty() {
        return Err(GpuSelectError::NoneAvailable);
    }
    let metadata: Vec<GpuAdapter> = adapters.iter().map(GpuAdapter::from_wgpu).collect();
    select_index(&metadata, gpu_device)
        .map(|idx| &adapters[idx])
        .ok_or_else(|| GpuSelectError::NoMatch {
            gpu_device: gpu_device.to_string(),
            available: metadata
                .iter()
                .map(|a| format!("  - {}", a))
                .collect::<Vec<_>>()
                .join("\n"),
        })
}

/// Pure selection logic operating on metadata-only adapter records.
///
/// Returns the index of the first adapter matching `gpu_device`, or
/// `None` if no match is found. Exposed at module visibility so the
/// round-trip test can verify the inverse of [`GpuAdapter::config_value`]
/// without needing live `wgpu::Adapter` instances.
fn select_index(adapters: &[GpuAdapter], gpu_device: &str) -> Option<usize> {
    let needle = gpu_device.to_lowercase();
    let target_kind = match needle.as_str() {
        "integrated" => Some(GpuKind::Integrated),
        "discrete" => Some(GpuKind::Discrete),
        _ => None,
    };
    if let Some(kind) = target_kind {
        adapters.iter().position(|a| a.kind == kind)
    } else {
        adapters
            .iter()
            .position(|a| a.name.to_lowercase().contains(&needle))
    }
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
    fn select_index_matches_kind_keyword() {
        let adapters = vec![
            adapter("AMD Radeon Graphics", GpuKind::Integrated),
            adapter("NVIDIA GeForce RTX 4070", GpuKind::Discrete),
        ];
        assert_eq!(select_index(&adapters, "integrated"), Some(0));
        assert_eq!(select_index(&adapters, "INTEGRATED"), Some(0));
        assert_eq!(select_index(&adapters, "discrete"), Some(1));
    }

    #[test]
    fn select_index_matches_name_substring_case_insensitive() {
        let adapters = vec![
            adapter("AMD Radeon Graphics", GpuKind::Integrated),
            adapter("NVIDIA GeForce RTX 4070", GpuKind::Discrete),
        ];
        assert_eq!(select_index(&adapters, "rtx"), Some(1));
        assert_eq!(select_index(&adapters, "RADEON"), Some(0));
    }

    #[test]
    fn select_index_returns_none_for_no_match() {
        let adapters = vec![adapter("Intel UHD 630", GpuKind::Integrated)];
        assert_eq!(select_index(&adapters, "discrete"), None);
        assert_eq!(select_index(&adapters, "nvidia"), None);
    }

    #[test]
    fn config_value_and_select_index_round_trip() {
        // For every adapter in every realistic enumeration shape, the
        // config_value we'd write to config.ini must round-trip back to
        // the same adapter via select_index. This is the SSOT guarantee
        // that pairs the wizard (config_value) with the encoder
        // (find_adapter / select_index).
        let scenarios = vec![
            // Typical multi-GPU laptop / workstation
            vec![
                adapter("AMD Radeon Graphics", GpuKind::Integrated),
                adapter("NVIDIA GeForce RTX 4070", GpuKind::Discrete),
            ],
            // Dual-discrete render farm
            vec![
                adapter("NVIDIA GeForce RTX 4070", GpuKind::Discrete),
                adapter("NVIDIA Quadro P2000", GpuKind::Discrete),
            ],
            // Single GPU
            vec![adapter("Intel Arc A770", GpuKind::Discrete)],
            // Headless / software fallback
            vec![adapter("llvmpipe (LLVM 17)", GpuKind::Cpu)],
            // Mixed kinds with a virtual adapter (cloud GPU passthrough)
            vec![
                adapter("Intel UHD Graphics 630", GpuKind::Integrated),
                adapter("Virtual GPU", GpuKind::Virtual),
                adapter("NVIDIA Tesla T4", GpuKind::Discrete),
            ],
        ];
        for adapters in scenarios {
            for (i, adapter) in adapters.iter().enumerate() {
                let cv = adapter.config_value(&adapters);
                let idx = select_index(&adapters, &cv).unwrap_or_else(|| {
                    panic!(
                        "config_value '{}' for {:?} did not round-trip via select_index",
                        cv, adapter
                    )
                });
                assert_eq!(
                    idx, i,
                    "config_value '{}' selected index {} but {:?} is at index {}",
                    cv, idx, adapter, i
                );
            }
        }
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
