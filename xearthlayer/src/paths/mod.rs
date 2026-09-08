//! Where XEarthLayer keeps its files.
//!
//! One resolver, four roles, three platform layouts. Everything that needs a
//! path asks here rather than constructing one, so relocating a role is a
//! change in one place instead of a search for every caller that guessed.

mod apple;
mod base;
mod legacy;
mod xdg;

pub mod testing;

pub use apple::AppleDirectories;
pub use base::BaseDirectories;
pub use legacy::LegacyDirectories;
pub use xdg::XdgDirectories;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn xdg_falls_back_to_the_spec_defaults_when_nothing_is_set() {
        let home = PathBuf::from("/home/pilot");
        let d = XdgDirectories::from_parts(home.clone(), None, None, None, None);
        assert_eq!(d.config_dir(), home.join(".config/xearthlayer"));
        assert_eq!(d.cache_dir(), home.join(".cache/xearthlayer"));
        assert_eq!(d.data_dir(), home.join(".local/share/xearthlayer"));
        assert_eq!(d.state_dir(), home.join(".local/state/xearthlayer"));
    }

    #[test]
    fn xdg_honours_the_environment_when_it_is_set() {
        let d = XdgDirectories::from_parts(
            PathBuf::from("/home/pilot"),
            Some(PathBuf::from("/xdg/config")),
            Some(PathBuf::from("/xdg/cache")),
            Some(PathBuf::from("/xdg/data")),
            Some(PathBuf::from("/xdg/state")),
        );
        assert_eq!(d.config_dir(), PathBuf::from("/xdg/config/xearthlayer"));
        assert_eq!(d.cache_dir(), PathBuf::from("/xdg/cache/xearthlayer"));
        assert_eq!(d.data_dir(), PathBuf::from("/xdg/data/xearthlayer"));
        assert_eq!(d.state_dir(), PathBuf::from("/xdg/state/xearthlayer"));
    }

    #[test]
    fn xdg_roles_are_four_distinct_directories() {
        let d = XdgDirectories::from_parts(PathBuf::from("/home/pilot"), None, None, None, None);
        let all = [d.config_dir(), d.cache_dir(), d.data_dir(), d.state_dir()];
        let mut unique = all.to_vec();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), 4, "XDG separates all four roles");
    }

    #[test]
    fn apple_collapses_config_and_data_and_puts_state_in_logs() {
        // dirs::config_dir() and dirs::data_dir() both return Application
        // Support on macOS, and dirs::state_dir() returns None there. Nothing
        // may assume the four roles are four distinct directories.
        let home = PathBuf::from("/Users/pilot");
        let d = AppleDirectories::from_home(home.clone());
        let app_support = home.join("Library/Application Support/XEarthLayer");
        assert_eq!(d.config_dir(), app_support);
        assert_eq!(d.data_dir(), app_support);
        assert_eq!(d.cache_dir(), home.join("Library/Caches/XEarthLayer"));
        assert_eq!(d.state_dir(), home.join("Library/Logs/XEarthLayer"));
    }

    #[test]
    fn apple_uses_the_capitalised_bundle_name() {
        let d = AppleDirectories::from_home(PathBuf::from("/Users/pilot"));
        assert!(
            d.config_dir().to_string_lossy().contains("XEarthLayer"),
            "macOS convention capitalises the application directory"
        );
    }

    #[test]
    fn legacy_puts_every_role_in_one_directory() {
        let home = PathBuf::from("/home/pilot");
        let d = LegacyDirectories::from_home(home.clone());
        let dot = home.join(".xearthlayer");
        assert_eq!(d.config_dir(), dot);
        assert_eq!(d.cache_dir(), dot);
        assert_eq!(d.data_dir(), dot);
        assert_eq!(d.state_dir(), dot);
    }

    #[test]
    fn a_trait_object_can_hold_any_implementation() {
        // Nothing may depend on the concrete type: the whole point is that the
        // active implementation is chosen once and read through the trait.
        let all: Vec<Box<dyn BaseDirectories>> = vec![
            Box::new(XdgDirectories::from_parts(
                PathBuf::from("/home/pilot"),
                None,
                None,
                None,
                None,
            )),
            Box::new(AppleDirectories::from_home(PathBuf::from("/Users/pilot"))),
            Box::new(LegacyDirectories::from_home(PathBuf::from("/home/pilot"))),
        ];
        for d in all {
            assert!(d.config_dir().is_absolute());
        }
    }
}
