//! Configuration: what `xearthlayer config` and `xearthlayer init` do.

use serde::Serialize;
use xearthlayer::config::{
    analyze_config, config_file_path, detect_scenery_dir, upgrade_config, ConfigFile, ConfigKey,
    SceneryDetectionResult,
};

use crate::environment::Environment;
use crate::views::blocking;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
    pub version: String,
    pub config_path: String,
    pub config_exists: bool,
    pub install_location: String,
    pub custom_scenery: Option<String>,
    pub log_file: String,
    pub platform: String,
}

#[tauri::command]
pub async fn app_info() -> Result<AppInfo, String> {
    blocking(|| {
        let env = Environment::load()?;
        Ok(AppInfo {
            version: xearthlayer::VERSION.to_string(),
            config_path: config_file_path().display().to_string(),
            config_exists: env.config_exists,
            install_location: env.install_location.display().to_string(),
            custom_scenery: env.custom_scenery.map(|p| p.display().to_string()),
            log_file: crate::gui_log_file().display().to_string(),
            platform: std::env::consts::OS.to_string(),
        })
    })
    .await
}

/// One setting, for the Settings page.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingView {
    /// `section.key`, what `config set` takes.
    pub name: String,
    pub section: String,
    pub key: String,
    /// The value; empty for a credential, whose value never leaves the backend.
    pub value: String,
    pub is_set: bool,
    pub sensitive: bool,
    pub help: Vec<String>,
}

#[tauri::command]
pub async fn config_list() -> Result<Vec<SettingView>, String> {
    blocking(|| {
        let config = Environment::load()?.config;
        Ok(ConfigKey::all()
            .iter()
            .map(|key| {
                let value = key.get(&config);
                SettingView {
                    name: key.name().to_string(),
                    section: key.section().to_string(),
                    key: key.key_name().to_string(),
                    is_set: !value.is_empty(),
                    value: if key.is_sensitive() {
                        String::new()
                    } else {
                        value
                    },
                    sensitive: key.is_sensitive(),
                    help: xearthlayer::config::help::help(*key),
                }
            })
            .collect())
    })
    .await
}

#[tauri::command]
pub async fn config_set(
    state: tauri::State<'_, crate::AppState>,
    key: String,
    value: String,
) -> Result<String, String> {
    if state.service.is_active() {
        return Err(
            "Stop XEarthLayer before changing settings; they apply on the next start.".into(),
        );
    }
    blocking(move || {
        let config_key: ConfigKey = key
            .parse()
            .map_err(|_| format!("Unknown configuration key '{key}'"))?;
        let mut config = Environment::load()?.config;
        config_key
            .set(&mut config, &value)
            .map_err(|e| e.to_string())?;
        config
            .save()
            .map_err(|e| format!("Failed to save the configuration: {e}"))?;
        Ok(format!("Set {}", config_key.name()))
    })
    .await
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpgradeView {
    pub exists: bool,
    pub needs_upgrade: bool,
    pub missing: Vec<String>,
    pub deprecated: Vec<String>,
    pub rewritten: Vec<String>,
    pub unknown: Vec<String>,
    pub applied: bool,
    pub backup: Option<String>,
}

/// `config upgrade`, or with `dry_run` only its analysis.
#[tauri::command]
pub async fn config_upgrade(dry_run: bool) -> Result<UpgradeView, String> {
    blocking(move || {
        let path = config_file_path();
        if !path.exists() {
            return Ok(UpgradeView::default());
        }
        let analysis = analyze_config(&path).map_err(|e| e.to_string())?;
        let mut rewritten: Vec<String> = analysis
            .folded_values
            .iter()
            .map(|f| f.describe())
            .collect();
        rewritten.extend(
            analysis
                .migrated_values
                .iter()
                .map(|r| format!("{} = {} → {}", r.key, r.value, r.replacement)),
        );
        let mut view = UpgradeView {
            exists: true,
            needs_upgrade: analysis.needs_upgrade,
            missing: analysis.missing_keys.clone(),
            deprecated: analysis.deprecated_keys.clone(),
            rewritten,
            unknown: analysis.unknown_keys.clone(),
            ..UpgradeView::default()
        };
        if !dry_run && analysis.needs_upgrade {
            let result = upgrade_config(&path, false).map_err(|e| e.to_string())?;
            view.applied = true;
            view.backup = result.backup_path.map(|p| p.display().to_string());
        }
        Ok(view)
    })
    .await
}

/// `xearthlayer init`: write the configuration, with the X-Plane scenery
/// folder when exactly one installation is found.
#[tauri::command]
pub async fn config_init() -> Result<String, String> {
    blocking(|| {
        let mut config = ConfigFile::load().unwrap_or_default();
        if config.xplane.scenery_dir.is_none() {
            if let SceneryDetectionResult::Single(path) = detect_scenery_dir() {
                config.xplane.scenery_dir = Some(path);
            }
        }
        config
            .save()
            .map_err(|e| format!("Failed to save the configuration: {e}"))?;
        Ok(config_file_path().display().to_string())
    })
    .await
}
