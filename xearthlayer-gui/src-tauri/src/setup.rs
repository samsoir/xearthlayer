//! First-time setup: what `xearthlayer setup` asks, as one form.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use xearthlayer::config::{detect_scenery_dir, ConfigFile, NetworkLevel, SceneryDetectionResult};
use xearthlayer::provider::catalog::PROVIDERS;
use xearthlayer::provider::ProviderCredential;
use xearthlayer::system::{
    enumerate_gpus, CompressorKind, DefaultEncoderPolicy, EncoderPolicy, HardwareProfile,
    SystemInfo,
};

use crate::environment::{custom_scenery, install_location};
use crate::views::blocking;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderView {
    pub key: String,
    pub name: String,
    pub summary: String,
    /// `google` or `mapbox` when the provider needs a credential.
    pub credential: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HardwareView {
    pub cpu_cores: usize,
    pub memory: String,
    pub storage: String,
    pub available_bytes: u64,
    pub recommended_memory_cache: u64,
    pub recommended_disk_cache: u64,
}

/// The form, filled with the current configuration (or the defaults).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupForm {
    pub scenery_dir: Option<String>,
    pub package_dir: String,
    pub cache_dir: String,
    pub memory_cache_size: u64,
    pub disk_cache_size: u64,
    pub dds_disk_ratio: f64,
    pub texture_compressor: String,
    pub texture_gpu_device: String,
    pub provider_type: String,
    /// Left empty to keep the saved credential.
    #[serde(default)]
    pub google_api_key: String,
    #[serde(default)]
    pub mapbox_access_token: String,
    pub xworld_pro: bool,
    pub network_connections: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupDetection {
    pub form: SetupForm,
    pub xplane_candidates: Vec<String>,
    pub providers: Vec<ProviderView>,
    pub hardware: HardwareView,
    pub has_google_key: bool,
    pub has_mapbox_token: bool,
    pub config_exists: bool,
}

fn level_name(level: NetworkLevel) -> String {
    level.name().to_string()
}

fn parse_level(name: &str) -> Result<NetworkLevel, String> {
    [NetworkLevel::Max, NetworkLevel::High, NetworkLevel::Low]
        .into_iter()
        .find(|l| l.name().eq_ignore_ascii_case(name))
        .ok_or_else(|| format!("Unknown network level '{name}'"))
}

#[tauri::command]
pub async fn setup_detect() -> Result<SetupDetection, String> {
    blocking(|| {
        let config_exists = xearthlayer::config::config_file_path().exists();
        let config = ConfigFile::load().unwrap_or_default();
        let candidates = match detect_scenery_dir() {
            SceneryDetectionResult::NotFound => Vec::new(),
            SceneryDetectionResult::Single(p) => vec![p],
            SceneryDetectionResult::Multiple(ps) => ps,
        };
        let scenery = custom_scenery(&config, || candidates.first().cloned().ok_or(()));
        let system = SystemInfo::detect(&config.cache.directory);
        Ok(SetupDetection {
            form: SetupForm {
                scenery_dir: scenery.map(|p| p.display().to_string()),
                package_dir: install_location(&config).display().to_string(),
                cache_dir: config.cache.directory.display().to_string(),
                memory_cache_size: config.cache.memory_size as u64,
                disk_cache_size: config.cache.disk_size as u64,
                dds_disk_ratio: config.cache.dds_disk_ratio,
                texture_compressor: config.texture.compressor.clone(),
                texture_gpu_device: config.texture.gpu_device.clone(),
                provider_type: config.provider.provider_type.clone(),
                google_api_key: String::new(),
                mapbox_access_token: String::new(),
                xworld_pro: config.packages.xworld_pro,
                network_connections: level_name(config.network.connections),
            },
            xplane_candidates: candidates.iter().map(|p| p.display().to_string()).collect(),
            providers: PROVIDERS
                .iter()
                .map(|p| ProviderView {
                    key: p.key.to_string(),
                    name: p.name.to_string(),
                    summary: p.summary.to_string(),
                    credential: p.credential.map(|c| match c {
                        ProviderCredential::GoogleApiKey => "google".to_string(),
                        ProviderCredential::MapboxAccessToken => "mapbox".to_string(),
                    }),
                })
                .collect(),
            hardware: HardwareView {
                cpu_cores: system.cpu_cores,
                memory: system.memory_display(),
                storage: system.storage_display().to_string(),
                available_bytes: system.cache_path_available_bytes,
                recommended_memory_cache: system.recommended_memory_cache() as u64,
                recommended_disk_cache: system.recommended_disk_cache() as u64,
            },
            has_google_key: config.provider.google_api_key.is_some(),
            has_mapbox_token: config.provider.mapbox_access_token.is_some(),
            config_exists,
        })
    })
    .await
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GpuView {
    pub name: String,
    /// What `texture.gpu_device` takes to select it.
    pub config_value: String,
    /// The policy's warning for choosing GPU encoding on this host.
    pub warning: Option<String>,
}

/// GPU adapters usable for encoding. Enumeration can take 30 s or more.
#[tauri::command]
pub async fn setup_gpus() -> Result<Vec<GpuView>, String> {
    blocking(|| {
        let hw = HardwareProfile::new(enumerate_gpus());
        let warning = DefaultEncoderPolicy
            .select(&hw, Some(CompressorKind::Gpu))
            .warning;
        Ok(hw
            .encoding_candidates()
            .iter()
            .map(|adapter| GpuView {
                name: adapter.to_string(),
                config_value: adapter.config_value(&hw.adapters),
                warning: warning.clone(),
            })
            .collect())
    })
    .await
}

/// Write the setup choices, as the wizard's `write_config` does.
#[tauri::command]
pub async fn setup_save(
    state: tauri::State<'_, crate::AppState>,
    form: SetupForm,
) -> Result<String, String> {
    if state.service.is_active() {
        return Err("Stop XEarthLayer before changing settings.".into());
    }
    blocking(move || {
        let mut config = ConfigFile::load().unwrap_or_default();
        apply_form(&mut config, &form)?;
        let package_dir = PathBuf::from(&form.package_dir);
        std::fs::create_dir_all(&package_dir).ok();
        std::fs::create_dir_all(&config.cache.directory).ok();
        config
            .save()
            .map_err(|e| format!("Failed to save the configuration: {e}"))?;
        Ok(xearthlayer::config::config_file_path()
            .display()
            .to_string())
    })
    .await
}

fn apply_form(config: &mut ConfigFile, form: &SetupForm) -> Result<(), String> {
    if form.package_dir.trim().is_empty() || form.cache_dir.trim().is_empty() {
        return Err("The package and cache folders are required.".into());
    }
    if !(0.0..=1.0).contains(&form.dds_disk_ratio) {
        return Err("The DDS share of the disk cache must be between 0 and 1.".into());
    }
    if CompressorKind::parse(&form.texture_compressor).is_none() {
        return Err(format!("Unknown encoder '{}'", form.texture_compressor));
    }
    let provider = PROVIDERS
        .iter()
        .find(|p| p.key == form.provider_type)
        .ok_or_else(|| format!("Unknown provider '{}'", form.provider_type))?;

    if let Some(dir) = form.scenery_dir.as_deref().filter(|s| !s.trim().is_empty()) {
        config.xplane.scenery_dir = Some(PathBuf::from(dir));
        config.packages.custom_scenery_path = Some(PathBuf::from(dir));
    }
    config.packages.install_location = Some(PathBuf::from(&form.package_dir));
    config.packages.xworld_pro = form.xworld_pro;
    config.network.connections = parse_level(&form.network_connections)?;
    config.cache.directory = PathBuf::from(&form.cache_dir);
    config.cache.memory_size = form.memory_cache_size as usize;
    config.cache.disk_size = form.disk_cache_size as usize;
    config.cache.dds_disk_ratio = form.dds_disk_ratio;
    config.texture.compressor = form.texture_compressor.clone();
    config.texture.gpu_device = form.texture_gpu_device.clone();
    config.provider.provider_type = provider.key.to_string();
    // Written only when entered, so a saved credential survives choosing a
    // provider that does not need it.
    if !form.google_api_key.trim().is_empty() {
        config.provider.google_api_key = Some(form.google_api_key.trim().to_string());
    }
    if !form.mapbox_access_token.trim().is_empty() {
        config.provider.mapbox_access_token = Some(form.mapbox_access_token.trim().to_string());
    }
    match provider.credential {
        Some(ProviderCredential::GoogleApiKey) if config.provider.google_api_key.is_none() => {
            Err("Google Maps needs an API key.".into())
        }
        Some(ProviderCredential::MapboxAccessToken)
            if config.provider.mapbox_access_token.is_none() =>
        {
            Err("MapBox needs an access token.".into())
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form() -> SetupForm {
        SetupForm {
            scenery_dir: Some("/xp/Custom Scenery".into()),
            package_dir: "/data/packages".into(),
            cache_dir: "/data/cache".into(),
            memory_cache_size: 1 << 30,
            disk_cache_size: 20 << 30,
            dds_disk_ratio: 0.6,
            texture_compressor: "ispc".into(),
            texture_gpu_device: "integrated".into(),
            provider_type: "bing".into(),
            google_api_key: String::new(),
            mapbox_access_token: String::new(),
            xworld_pro: false,
            network_connections: "high".into(),
        }
    }

    #[test]
    fn the_form_writes_the_wizard_settings() {
        let mut config = ConfigFile::default();
        apply_form(&mut config, &form()).unwrap();
        assert_eq!(
            config.packages.custom_scenery_path,
            Some(PathBuf::from("/xp/Custom Scenery"))
        );
        assert_eq!(
            config.xplane.scenery_dir,
            config.packages.custom_scenery_path
        );
        assert_eq!(config.cache.disk_size, 20 << 30);
        assert_eq!(config.network.connections, NetworkLevel::High);
    }

    #[test]
    fn an_empty_credential_keeps_the_saved_one() {
        let mut config = ConfigFile::default();
        config.provider.google_api_key = Some("saved".into());
        let mut f = form();
        f.provider_type = "google".into();
        apply_form(&mut config, &f).unwrap();
        assert_eq!(config.provider.google_api_key.as_deref(), Some("saved"));
    }

    #[test]
    fn a_credential_provider_without_its_credential_is_refused() {
        let mut config = ConfigFile::default();
        config.provider.mapbox_access_token = None;
        let mut f = form();
        f.provider_type = "mapbox".into();
        assert!(apply_form(&mut config, &f).is_err());
    }

    #[test]
    fn unknown_values_are_refused() {
        let mut config = ConfigFile::default();
        let mut f = form();
        f.network_connections = "turbo".into();
        assert!(apply_form(&mut config, &f).is_err());
        let mut f = form();
        f.provider_type = "nope".into();
        assert!(apply_form(&mut config, &f).is_err());
    }
}
