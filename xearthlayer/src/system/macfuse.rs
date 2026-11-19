//! The installed macFUSE: where its bundle lives and which version it is.
//!
//! macOS mounts XEarthLayer's volume through macFUSE's FSKit backend, so a
//! support report and a flight log both need to say which macFUSE served it
//! (#362). The version is read from the bundle's `Info.plist`, an XML
//! property list, with a small reader rather than a plist dependency: one
//! string value under one key is all that is needed.
//!
//! Compiled on every platform, so the parsing is tested on Linux too; the
//! readers that touch the disk answer `None` off macOS.

use std::path::Path;

/// macFUSE's filesystem bundle. Its presence means macFUSE is installed.
pub const MACFUSE_BUNDLE: &str = "/Library/Filesystems/macfuse.fs";

/// The bundle's property list, which carries the macFUSE version.
pub const MACFUSE_INFO_PLIST: &str = "/Library/Filesystems/macfuse.fs/Contents/Info.plist";

/// The `MFMount` library inside macFUSE's filesystem bundle, which the FSKit
/// mount loads (macFUSE 5.3.3 or later).
pub const MACFUSE_MFMOUNT: &str =
    "/Library/Filesystems/macfuse.fs/Contents/Frameworks/MFMount.framework/MFMount";

/// The macFUSE version in the text of its bundle's `Info.plist`:
/// `CFBundleShortVersionString`, the version macFUSE announces (`5.5.0`),
/// falling back to `CFBundleVersion`. `None` when neither is there, which
/// includes a binary property list.
pub fn macfuse_version_in(info_plist: &str) -> Option<String> {
    plist_string(info_plist, "CFBundleShortVersionString")
        .or_else(|| plist_string(info_plist, "CFBundleVersion"))
        .map(str::to_string)
}

/// The installed macFUSE version, read from [`MACFUSE_INFO_PLIST`]. `None`
/// off macOS, when macFUSE is not installed, or when the version cannot be
/// read.
pub fn installed_macfuse_version() -> Option<String> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let info_plist = std::fs::read_to_string(Path::new(MACFUSE_INFO_PLIST)).ok()?;
    macfuse_version_in(&info_plist)
}

/// The `<string>` value that follows `<key>key</key>` in an XML property
/// list, trimmed; `None` when the key is missing, is not followed by a
/// string, or the string is empty.
fn plist_string<'a>(plist: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("<key>{key}</key>");
    let after_key = &plist[plist.find(&needle)? + needle.len()..];
    let value = after_key.trim_start().strip_prefix("<string>")?;
    let value = value[..value.find("</string>")?].trim();
    (!value.is_empty()).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A property list shaped like `macfuse.fs/Contents/Info.plist`.
    const INFO_PLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleDevelopmentRegion</key>
	<string>English</string>
	<key>CFBundleIdentifier</key>
	<string>io.macfuse.filesystems.macfuse</string>
	<key>CFBundleName</key>
	<string>macFUSE</string>
	<key>CFBundlePackageType</key>
	<string>BNDL</string>
	<key>CFBundleShortVersionString</key>
	<string>5.5.0</string>
	<key>CFBundleVersion</key>
	<string>5.5.0</string>
	<key>FSPersonalities</key>
	<dict/>
</dict>
</plist>
"#;

    #[test]
    fn the_version_is_the_short_version_string() {
        assert_eq!(macfuse_version_in(INFO_PLIST).as_deref(), Some("5.5.0"));
    }

    #[test]
    fn the_short_version_string_wins_over_the_bundle_version() {
        let plist = "<key>CFBundleVersion</key><string>1934</string>\n\
                     <key>CFBundleShortVersionString</key>\n\t<string> 5.3.3 </string>";
        assert_eq!(macfuse_version_in(plist).as_deref(), Some("5.3.3"));
    }

    #[test]
    fn the_bundle_version_is_the_fallback() {
        let plist = "<dict>\n\t<key>CFBundleVersion</key>\n\t<string>4.10.2</string>\n</dict>";
        assert_eq!(macfuse_version_in(plist).as_deref(), Some("4.10.2"));
    }

    #[test]
    fn no_version_is_read_from_a_list_without_one() {
        assert_eq!(macfuse_version_in(""), None);
        assert_eq!(
            macfuse_version_in("<key>CFBundleShortVersionString</key><string></string>"),
            None
        );
        // A key whose value is not a string is not a version.
        assert_eq!(
            macfuse_version_in("<key>CFBundleShortVersionString</key><true/><string>x</string>"),
            None
        );
        // A binary property list.
        assert_eq!(macfuse_version_in("bplist00\u{1}\u{2}"), None);
    }

    /// Off macOS there is no macFUSE to read.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn no_version_is_installed_off_macos() {
        assert_eq!(installed_macfuse_version(), None);
    }
}
