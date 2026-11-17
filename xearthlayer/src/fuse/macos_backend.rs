//! Which macFUSE backend mounts the ortho volume on macOS (#210).
//!
//! macFUSE 5 serves a volume either through its kernel extension or through
//! Apple's FSKit, which needs no kext and so none of the Recovery mode
//! approval Apple Silicon requires for one. The filesystem is the same either
//! way; only fuse3's transport differs. Compiled on every platform so the
//! setting parses and is tested everywhere; it only takes effect on macOS.

use std::fmt;
use std::str::FromStr;

/// The `fuse.macos_backend` setting.
///
/// `Auto` is the default: every 0.5.0 Mac has the kernel extension approved
/// and few have turned on the File System Extension, so it tries FSKit, logs
/// why it could not mount, and falls back to the kext. The default can move
/// to `MacFuseFsKit` once that backend has flight time behind it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MacosBackend {
    /// macFUSE's FSKit backend (macFUSE 5.3.3+, File System Extensions on).
    MacFuseFsKit,
    /// The macFUSE kernel extension.
    Kernel,
    /// FSKit when it can mount, the kernel extension otherwise.
    #[default]
    Auto,
}

impl MacosBackend {
    /// The setting's value as written in `config.ini`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MacFuseFsKit => "macfuse-fskit",
            Self::Kernel => "kernel",
            Self::Auto => "auto",
        }
    }
}

impl fmt::Display for MacosBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A `fuse.macos_backend` value that names no backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownMacosBackend(pub String);

impl fmt::Display for UnknownMacosBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown macOS backend '{}': expected macfuse-fskit, kernel or auto",
            self.0
        )
    }
}

impl std::error::Error for UnknownMacosBackend {}

impl FromStr for MacosBackend {
    type Err = UnknownMacosBackend;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "macfuse-fskit" => Ok(Self::MacFuseFsKit),
            "kernel" => Ok(Self::Kernel),
            "auto" => Ok(Self::Auto),
            _ => Err(UnknownMacosBackend(s.to_string())),
        }
    }
}

#[cfg(target_os = "macos")]
impl From<MacosBackend> for fuse3::MacFuseBackend {
    fn from(backend: MacosBackend) -> Self {
        match backend {
            MacosBackend::MacFuseFsKit => fuse3::MacFuseBackend::FsKit,
            MacosBackend::Kernel => fuse3::MacFuseBackend::Kernel,
            MacosBackend::Auto => fuse3::MacFuseBackend::Auto,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::defaults::{DEFAULT_FUSE_MACOS_BACKEND, FUSE_MACOS_BACKEND_VALUES};

    #[test]
    fn the_default_is_auto() {
        // Not macfuse-fskit: a 0.5.0 install has the kext approved and almost never
        // the File System Extension, so a macfuse-fskit default would turn a working
        // install into a failed mount on upgrade.
        assert_eq!(MacosBackend::default(), MacosBackend::Auto);
    }

    #[test]
    fn every_config_value_parses_and_round_trips() {
        for value in FUSE_MACOS_BACKEND_VALUES {
            let backend: MacosBackend = value.parse().unwrap();
            assert_eq!(backend.as_str(), *value);
        }
    }

    #[test]
    fn the_config_default_parses_to_the_type_default() {
        let backend: MacosBackend = DEFAULT_FUSE_MACOS_BACKEND.parse().unwrap();
        assert_eq!(backend, MacosBackend::default());
    }

    #[test]
    fn parsing_ignores_case_and_surrounding_space() {
        assert_eq!(" Kernel ".parse(), Ok(MacosBackend::Kernel));
        assert_eq!("macFUSE-fskit".parse(), Ok(MacosBackend::MacFuseFsKit));
    }

    #[test]
    fn plain_fskit_is_refused_because_it_is_reserved_for_native_fskit() {
        // `fskit` will name the native FSKit backend (no macFUSE, #361);
        // macFUSE's FSKit backend is `macfuse-fskit`.
        assert!("fskit".parse::<MacosBackend>().is_err());
        assert!(!FUSE_MACOS_BACKEND_VALUES.contains(&"fskit"));
    }

    #[test]
    fn an_unknown_value_is_refused_and_named() {
        let err = "kext".parse::<MacosBackend>().unwrap_err();
        assert!(err.to_string().contains("kext"), "{err}");
    }
}
