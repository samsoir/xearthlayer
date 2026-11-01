//! Help text for configuration settings, the one source for the comments in
//! `config.ini` and for the package manager's Settings tab (#336).
//!
//! Each setting's help is a few paragraphs. `config.ini` wraps them into `;`
//! comment lines; the Settings tab shows them as they are. A paragraph may
//! hold `{default}`, replaced by the setting's resolved default.

use super::keys::ConfigKey;

/// The `[packages]` settings, in the order `config.ini` lists them.
pub const PACKAGES_SETTINGS: [ConfigKey; 8] = [
    ConfigKey::PackagesLibraryUrl,
    ConfigKey::PackagesInstallLocation,
    ConfigKey::PackagesCustomSceneryPath,
    ConfigKey::PackagesAutoInstallOverlays,
    ConfigKey::PackagesDisableOverlays,
    ConfigKey::PackagesXworldPro,
    ConfigKey::PackagesTempDir,
    ConfigKey::PackagesConcurrentDownloads,
];

/// Width `config.ini` wraps help comments to, the `; ` included.
const COMMENT_WIDTH: usize = 80;

/// The help paragraphs of `key`, before `{default}` is resolved.
fn paragraphs(key: ConfigKey) -> &'static [&'static str] {
    match key {
        ConfigKey::PackagesLibraryUrl => &[
            "URL to the XEarthLayer package library index. This is where available packages \
             are discovered for 'xearthlayer packages check/install'.",
        ],
        ConfigKey::PackagesInstallLocation => &[
            "Local directory for installed packages.",
            "Default: {default}",
        ],
        ConfigKey::PackagesCustomSceneryPath => &[
            "X-Plane Custom Scenery directory for overlay symlinks. If empty, uses \
             xplane.scenery_dir or auto-detects.",
        ],
        ConfigKey::PackagesAutoInstallOverlays => {
            &["Automatically install overlay packages when installing ortho for the same region."]
        }
        ConfigKey::PackagesDisableOverlays => &[
            "Suppress XEL overlays at runtime by removing the consolidated yzXEL_overlay/ \
             symlink folder from Custom Scenery on `xearthlayer run` startup. Overlay packages \
             are still downloaded and stored locally; only the symlink that X-Plane reads is \
             suppressed. Use this when running third-party overlay scenery (e.g., SimHeaven) \
             that conflicts with XEL overlays. (default: false)",
        ],
        ConfigKey::PackagesXworldPro => &[
            "Give SimHeaven X-World Pro priority over XEarthLayer overlays. On every \
             `xearthlayer run`, overlays are left out where X-World Pro is active and the \
             scenery order and X-World Pro setup are checked. Set by `xearthlayer setup`. \
             (default: false)",
        ],
        ConfigKey::PackagesTempDir => &[
            "Temporary directory for package downloads.",
            "Default: {default}",
            "Large packages are downloaded here before extraction.",
        ],
        ConfigKey::PackagesConcurrentDownloads => {
            &["Number of concurrent part downloads (1-10, default: 5)."]
        }
        _ => &[],
    }
}

/// The resolved default `{default}` stands for, for the settings that have one.
fn default_text(key: ConfigKey) -> Option<String> {
    let path = match key {
        ConfigKey::PackagesInstallLocation => crate::paths::packages_dir(),
        ConfigKey::PackagesTempDir => crate::paths::temp_dir(),
        _ => return None,
    };
    Some(super::writer::path_to_string(&path))
}

/// The help of `key` as paragraphs, defaults resolved. Empty for a setting
/// with no help here.
pub fn help(key: ConfigKey) -> Vec<String> {
    let default = default_text(key);
    paragraphs(key)
        .iter()
        .map(|p| match &default {
            Some(d) => p.replace("{default}", d),
            None => p.to_string(),
        })
        .collect()
}

/// The help of `key` as `config.ini` comment lines, without a trailing newline.
pub(super) fn ini_comment(key: ConfigKey) -> String {
    help(key)
        .iter()
        // A default is a path: one line, however long, never split from its label.
        .flat_map(|p| {
            if p.starts_with("Default: ") {
                vec![p.clone()]
            } else {
                wrap(p, COMMENT_WIDTH - 2)
            }
        })
        .map(|line| format!("; {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Greedy word wrap. A word longer than `width` gets a line of its own.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && line.len() + 1 + word.len() > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_packages_setting_has_help_and_no_other_packages_key_is_missing() {
        for key in PACKAGES_SETTINGS {
            assert!(!help(key).is_empty(), "{}", key.name());
        }
        let in_section: Vec<ConfigKey> = ConfigKey::all()
            .iter()
            .copied()
            .filter(|k| k.section() == "packages")
            .collect();
        assert_eq!(
            in_section.len(),
            PACKAGES_SETTINGS.len(),
            "a [packages] key without help"
        );
    }

    #[test]
    fn defaults_are_resolved_into_the_help() {
        let install = help(ConfigKey::PackagesInstallLocation).join("\n");
        assert!(!install.contains("{default}"));
        assert!(install.contains(&super::super::writer::path_to_string(
            &crate::paths::packages_dir()
        )));
    }

    #[test]
    fn ini_comments_wrap_within_the_width_and_carry_every_word() {
        for key in PACKAGES_SETTINGS {
            let comment = ini_comment(key);
            for line in comment.lines() {
                assert!(line.starts_with("; "), "{line:?}");
                // A default path may be long; nothing else is.
                assert!(
                    line.len() <= COMMENT_WIDTH || line.starts_with("; Default: "),
                    "{} line too long: {line:?}",
                    key.name()
                );
            }
            let words = |s: &str| {
                s.split_whitespace()
                    .filter(|w| *w != ";")
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            assert_eq!(
                words(&comment),
                words(&help(key).join(" ")),
                "{}",
                key.name()
            );
        }
    }

    #[test]
    fn wrap_breaks_between_words() {
        assert_eq!(wrap("a bb ccc dddd", 6), ["a bb", "ccc", "dddd"]);
        assert_eq!(
            wrap("averyveryverylongword x", 5),
            ["averyveryverylongword", "x"]
        );
        assert!(wrap("", 10).is_empty());
    }
}
