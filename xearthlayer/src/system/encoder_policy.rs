//! Which DDS encoder backend a host should use (#203).
//!
//! The choice used to be a config string plus an inline heuristic in the
//! setup wizard, and nothing stood between `texture.compressor = gpu` and a
//! machine whose only GPU is the one X-Plane renders with. Apple Silicon is
//! the motivating case: one unified GPU shared with the sim's Metal
//! renderer, and no Apple-native BCn encoder to make the contention worth
//! it. A single-GPU PC is the same case with a different driver.
//!
//! [`EncoderPolicy`] is the one place that rule lives. It is pure: it takes a
//! [`HardwareProfile`] built from the adapters wgpu already enumerated, so
//! applying it never costs a second enumeration (which can take ~30 s), and
//! it is injected rather than `#[cfg]`-gated, so every platform's behaviour
//! is tested on every platform.

use std::fmt;

use super::gpu::{GpuAdapter, GpuKind};

/// Backend a tile encoder uses: the `texture.compressor` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressorKind {
    /// Pure-Rust block compression, the fallback.
    Software,
    /// Intel ISPC SIMD kernels, the default on every platform.
    Ispc,
    /// wgpu compute shaders on a GPU.
    Gpu,
}

impl CompressorKind {
    /// Parse a `texture.compressor` value. `None` for anything unknown.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "software" => Some(Self::Software),
            "ispc" => Some(Self::Ispc),
            "gpu" => Some(Self::Gpu),
            _ => None,
        }
    }

    /// The `texture.compressor` value that selects this backend.
    pub fn config_value(self) -> &'static str {
        match self {
            Self::Software => "software",
            Self::Ispc => "ispc",
            Self::Gpu => "gpu",
        }
    }
}

impl fmt::Display for CompressorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.config_value())
    }
}

/// What the host offers an encoder: every GPU adapter wgpu enumerated.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HardwareProfile {
    pub adapters: Vec<GpuAdapter>,
}

impl HardwareProfile {
    pub fn new(adapters: Vec<GpuAdapter>) -> Self {
        Self { adapters }
    }

    /// The host has exactly one GPU, so X-Plane renders on it and any GPU
    /// encoding competes with the simulator for its execution units and
    /// memory bandwidth. True for every Apple Silicon Mac (one unified GPU,
    /// which Metal reports as integrated) and for a PC with a lone
    /// integrated or lone discrete GPU. A software rasteriser or a virtual
    /// adapter is not a render GPU, so it does not count.
    pub fn shares_render_gpu(&self) -> bool {
        matches!(
            self.adapters.as_slice(),
            [only] if matches!(only.kind, GpuKind::Integrated | GpuKind::Discrete)
        )
    }

    /// The GPUs the setup wizard may offer for encoding. None when the one
    /// GPU is shared with the sim; otherwise all of them, because with two
    /// or more there is no telling from here which one X-Plane renders on,
    /// and the wizard says so.
    pub fn encoding_candidates(&self) -> &[GpuAdapter] {
        if self.adapters.len() < 2 {
            &[]
        } else {
            &self.adapters
        }
    }
}

/// The policy's answer: the backend to use and, when honouring a choice
/// that will contend with the sim, a warning for the log and dashboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderDecision {
    pub backend: CompressorKind,
    pub warning: Option<String>,
}

/// Selects an encoder backend from the host profile and the user's
/// explicit preference, if any.
pub trait EncoderPolicy: Send + Sync {
    fn select(&self, hw: &HardwareProfile, user_pref: Option<CompressorKind>) -> EncoderDecision;
}

/// The shipped rules: honour an explicit preference, warn when it is `gpu`
/// on a shared-GPU host, and choose ISPC when there is no preference. GPU
/// is never auto-selected.
#[derive(Debug, Default, Clone, Copy)]
pub struct DefaultEncoderPolicy;

impl EncoderPolicy for DefaultEncoderPolicy {
    fn select(&self, hw: &HardwareProfile, user_pref: Option<CompressorKind>) -> EncoderDecision {
        match user_pref {
            Some(CompressorKind::Gpu) if hw.shares_render_gpu() => {
                let gpu = &hw.adapters[0];
                EncoderDecision {
                    backend: CompressorKind::Gpu,
                    warning: Some(format!(
                        "GPU acceleration is on {} ({}), the only GPU, which X-Plane renders \
                         with: encoding will compete with the simulator for it. Set \
                         texture.compressor = ispc to encode on the CPU, or keep gpu only for \
                         prewarming while X-Plane is closed.",
                        gpu.name,
                        gpu.kind.display().to_lowercase()
                    )),
                }
            }
            Some(backend) => EncoderDecision {
                backend,
                warning: None,
            },
            None => EncoderDecision {
                backend: CompressorKind::Ispc,
                warning: None,
            },
        }
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

    fn apple_silicon() -> HardwareProfile {
        HardwareProfile::new(vec![GpuAdapter {
            name: "Apple M2 Max".to_string(),
            kind: GpuKind::Integrated,
            backend: "metal".to_string(),
        }])
    }

    fn lone_discrete() -> HardwareProfile {
        HardwareProfile::new(vec![adapter("NVIDIA GeForce RTX 4070", GpuKind::Discrete)])
    }

    fn igpu_and_dgpu() -> HardwareProfile {
        HardwareProfile::new(vec![
            adapter("AMD Radeon 780M", GpuKind::Integrated),
            adapter("NVIDIA GeForce RTX 4070", GpuKind::Discrete),
        ])
    }

    #[test]
    fn compressor_kind_round_trips_through_its_config_value() {
        for kind in [
            CompressorKind::Software,
            CompressorKind::Ispc,
            CompressorKind::Gpu,
        ] {
            assert_eq!(CompressorKind::parse(kind.config_value()), Some(kind));
            assert_eq!(kind.to_string(), kind.config_value());
        }
        assert_eq!(CompressorKind::parse("gpu"), Some(CompressorKind::Gpu));
        assert_eq!(CompressorKind::parse("cuda"), None);
        assert_eq!(CompressorKind::parse(""), None);
    }

    #[test]
    fn a_lone_gpu_is_shared_with_the_sim_whatever_its_kind() {
        assert!(apple_silicon().shares_render_gpu());
        assert!(lone_discrete().shares_render_gpu());
        assert!(!igpu_and_dgpu().shares_render_gpu());
        assert!(!HardwareProfile::default().shares_render_gpu());
    }

    #[test]
    fn a_lone_software_or_virtual_adapter_is_not_a_render_gpu() {
        let llvmpipe = HardwareProfile::new(vec![adapter("llvmpipe (LLVM 17)", GpuKind::Cpu)]);
        assert!(!llvmpipe.shares_render_gpu());
        let virtio = HardwareProfile::new(vec![adapter("virtio-gpu", GpuKind::Virtual)]);
        assert!(!virtio.shares_render_gpu());
    }

    #[test]
    fn the_wizard_is_offered_gpus_only_when_there_are_at_least_two() {
        assert!(apple_silicon().encoding_candidates().is_empty());
        assert!(lone_discrete().encoding_candidates().is_empty());
        assert!(HardwareProfile::default().encoding_candidates().is_empty());
        assert_eq!(igpu_and_dgpu().encoding_candidates().len(), 2);
    }

    #[test]
    fn no_preference_selects_ispc_and_never_gpu() {
        for hw in [
            apple_silicon(),
            lone_discrete(),
            igpu_and_dgpu(),
            HardwareProfile::default(),
        ] {
            let decision = DefaultEncoderPolicy.select(&hw, None);
            assert_eq!(decision.backend, CompressorKind::Ispc, "{hw:?}");
            assert_eq!(decision.warning, None, "{hw:?}");
        }
    }

    #[test]
    fn gpu_on_a_shared_gpu_host_is_honoured_with_a_warning_naming_the_gpu() {
        let decision = DefaultEncoderPolicy.select(&apple_silicon(), Some(CompressorKind::Gpu));
        assert_eq!(
            decision.backend,
            CompressorKind::Gpu,
            "the choice is honoured"
        );
        let warning = decision.warning.expect("a shared GPU warns");
        assert!(warning.contains("Apple M2 Max"), "names the GPU: {warning}");
        assert!(
            warning.contains("texture.compressor = ispc"),
            "says how to fix it: {warning}"
        );

        let decision = DefaultEncoderPolicy.select(&lone_discrete(), Some(CompressorKind::Gpu));
        assert!(decision.warning.is_some(), "a lone discrete GPU warns too");
    }

    #[test]
    fn gpu_with_a_second_gpu_present_is_honoured_silently() {
        let decision = DefaultEncoderPolicy.select(&igpu_and_dgpu(), Some(CompressorKind::Gpu));
        assert_eq!(
            decision,
            EncoderDecision {
                backend: CompressorKind::Gpu,
                warning: None,
            }
        );
    }

    #[test]
    fn cpu_backends_are_honoured_silently_everywhere() {
        for kind in [CompressorKind::Ispc, CompressorKind::Software] {
            for hw in [apple_silicon(), lone_discrete(), igpu_and_dgpu()] {
                let decision = DefaultEncoderPolicy.select(&hw, Some(kind));
                assert_eq!(decision.backend, kind);
                assert_eq!(decision.warning, None);
            }
        }
    }
}
