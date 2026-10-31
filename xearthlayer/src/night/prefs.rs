//! X-Plane's texture quality setting, which decides how many top mip levels
//! of a lit texture are blank (the distance fade).
//!
//! Read once per session: a lit file's size then never changes while XEL
//! runs, so the kernel's attribute cache cannot serve a stale size. A
//! watching implementation of `TextureSettingSource` can replace this later.

use std::path::Path;

use tracing::warn;

/// The full texture setting, assumed when the preferences cannot be read.
pub const FULL_TEXTURE_SETTING: u8 = 4;

const PREF_KEY: &str = "renopt_tex_res_04";

/// Where the texture setting comes from.
pub trait TextureSettingSource: Send + Sync {
    /// X-Plane's texture quality, 0 (lowest) to 4 (full).
    fn texture_setting(&self) -> u8;
}

/// The setting read from `X-Plane.prf` when XEL started.
#[derive(Debug, Clone, Copy)]
pub struct StartupTextureSetting(u8);

impl StartupTextureSetting {
    /// Read `renopt_tex_res_04`; the full setting if absent or invalid (R16).
    pub fn read(prefs_path: &Path) -> Self {
        let value = std::fs::read_to_string(prefs_path).ok().and_then(|text| {
            text.lines().find_map(|line| {
                let mut tokens = line.split_whitespace();
                (tokens.next() == Some(PREF_KEY))
                    .then(|| tokens.next()?.parse::<u8>().ok())
                    .flatten()
            })
        });
        match value {
            Some(v) if v <= FULL_TEXTURE_SETTING => Self(v),
            _ => {
                warn!(path = %prefs_path.display(),
                      "Night lighting: texture setting not readable; assuming full");
                Self(FULL_TEXTURE_SETTING)
            }
        }
    }
}

impl TextureSettingSource for StartupTextureSetting {
    fn texture_setting(&self) -> u8 {
        self.0
    }
}

/// A setting known in advance.
#[derive(Debug, Clone, Copy)]
pub struct FixedTextureSetting(pub u8);

impl TextureSettingSource for FixedTextureSetting {
    fn texture_setting(&self) -> u8 {
        self.0.min(FULL_TEXTURE_SETTING)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn prefs(text: &str) -> (TempDir, PathBuf) {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("X-Plane.prf");
        std::fs::write(&path, text).unwrap();
        (temp, path)
    }

    #[test]
    fn reads_the_texture_setting() {
        let (_t, path) = prefs("I\n1100 Version\nrenopt_tex_res_04 2\nvr_renopt_tex_res_04 4\n");
        assert_eq!(StartupTextureSetting::read(&path).texture_setting(), 2);
    }

    #[test]
    fn ignores_the_vr_setting() {
        let (_t, path) = prefs("vr_renopt_tex_res_04 1\nrenopt_tex_res_04 3\n");
        assert_eq!(StartupTextureSetting::read(&path).texture_setting(), 3);
    }

    #[test]
    fn a_missing_file_or_bad_value_means_full() {
        assert_eq!(
            StartupTextureSetting::read(Path::new("/nonexistent.prf")).texture_setting(),
            4
        );
        let (_t, path) = prefs("renopt_tex_res_04 9\n");
        assert_eq!(StartupTextureSetting::read(&path).texture_setting(), 4);
        let (_t2, path2) = prefs("nothing here\n");
        assert_eq!(StartupTextureSetting::read(&path2).texture_setting(), 4);
    }

    #[test]
    fn fixed_setting_is_what_it_says() {
        assert_eq!(FixedTextureSetting(1).texture_setting(), 1);
    }
}
