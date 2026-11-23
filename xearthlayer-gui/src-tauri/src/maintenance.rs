//! Maintenance: `xearthlayer cache`, `scenery-index`, `patches` and
//! `diagnostics`.

use std::path::PathBuf;

use serde::Serialize;
use xearthlayer::cache::{clear_disk_cache, disk_cache_stats};
use xearthlayer::diagnostics::SystemReport;
use xearthlayer::manager::LocalPackageStore;
use xearthlayer::patches::PatchDiscovery;
use xearthlayer::prefetch::{
    cache_status, save_cache, scenery_cache_path, SceneryIndex, SceneryIndexConfig,
};
use xearthlayer_package::PackageType;

use crate::environment::Environment;
use crate::views::blocking;
use crate::AppState;

fn refuse_while_running(state: &AppState, what: &str) -> Result<(), String> {
    if state.service.is_active() {
        Err(format!("Stop XEarthLayer before {what}."))
    } else {
        Ok(())
    }
}

// ============================================================================
// Disk cache
// ============================================================================

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheStatsView {
    pub directory: String,
    pub files: u64,
    pub bytes: u64,
    pub budget_bytes: u64,
}

#[tauri::command]
pub async fn cache_stats() -> Result<CacheStatsView, String> {
    blocking(|| {
        let config = Environment::load()?.config;
        let dir = config.cache.directory.clone();
        let (files, bytes) = if dir.exists() {
            disk_cache_stats(&dir).map_err(|e| e.to_string())?
        } else {
            (0, 0)
        };
        Ok(CacheStatsView {
            directory: dir.display().to_string(),
            files: files as u64,
            bytes,
            budget_bytes: config.cache.disk_size as u64,
        })
    })
    .await
}

#[tauri::command]
pub async fn cache_clear(state: tauri::State<'_, AppState>) -> Result<String, String> {
    refuse_while_running(&state, "clearing the cache")?;
    blocking(|| {
        let config = Environment::load()?.config;
        let result = clear_disk_cache(&config.cache.directory).map_err(|e| e.to_string())?;
        Ok(format!(
            "Deleted {} files, freed {}",
            result.files_deleted,
            xearthlayer::config::format_size(result.bytes_freed as usize)
        ))
    })
    .await
}

// ============================================================================
// Scenery index
// ============================================================================

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexPackageView {
    pub name: String,
    pub terrain_files: usize,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SceneryIndexView {
    pub path: String,
    pub exists: bool,
    pub valid: bool,
    pub error: Option<String>,
    pub version: Option<String>,
    pub packages: Vec<IndexPackageView>,
    pub total_tiles: usize,
    pub sea_tiles: usize,
    pub file_bytes: u64,
    pub expected_tiles: usize,
}

#[tauri::command]
pub async fn scenery_index_status() -> Result<SceneryIndexView, String> {
    blocking(|| {
        let path = scenery_cache_path();
        let mut view = SceneryIndexView {
            path: path.display().to_string(),
            exists: path.exists(),
            ..SceneryIndexView::default()
        };
        if !view.exists {
            return Ok(view);
        }
        view.file_bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        match cache_status() {
            Ok(status) => {
                view.valid = true;
                view.version = Some(status.version.to_string());
                view.total_tiles = status.total_tiles;
                view.sea_tiles = status.sea_tiles;
                view.expected_tiles = status.packages.iter().map(|p| p.terrain_file_count).sum();
                view.packages = status
                    .packages
                    .iter()
                    .map(|p| IndexPackageView {
                        name: p.name.clone(),
                        terrain_files: p.terrain_file_count,
                    })
                    .collect();
            }
            Err(e) => view.error = Some(e.to_string()),
        }
        Ok(view)
    })
    .await
}

#[tauri::command]
pub async fn scenery_index_update(state: tauri::State<'_, AppState>) -> Result<String, String> {
    refuse_while_running(&state, "rebuilding the scenery index")?;
    blocking(|| {
        let env = Environment::load()?;
        let store = LocalPackageStore::new(&env.install_location);
        let packages: Vec<(String, PathBuf)> = store
            .list()
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter(|p| p.package_type() == PackageType::Ortho)
            .map(|p| (p.region().to_string(), p.path))
            .collect();
        if packages.is_empty() {
            return Err("No ortho packages are installed.".into());
        }
        let index = SceneryIndex::new(SceneryIndexConfig::default());
        let mut failures = Vec::new();
        for (name, path) in &packages {
            if let Err(e) = index.build_from_package(path) {
                failures.push(format!("{name}: {e}"));
            }
        }
        save_cache(&index, &packages).map_err(|e| e.to_string())?;
        let mut message = format!(
            "{} tiles indexed ({} land, {} sea)",
            index.tile_count(),
            index.tile_count() - index.sea_tile_count(),
            index.sea_tile_count()
        );
        if !failures.is_empty() {
            message.push_str(&format!("; errors: {}", failures.join(", ")));
        }
        Ok(message)
    })
    .await
}

#[tauri::command]
pub async fn scenery_index_clear(state: tauri::State<'_, AppState>) -> Result<String, String> {
    refuse_while_running(&state, "deleting the scenery index")?;
    blocking(|| {
        let path = scenery_cache_path();
        if !path.exists() {
            return Ok("No scenery index cache to delete".into());
        }
        std::fs::remove_file(&path).map_err(|e| format!("Failed to delete the cache: {e}"))?;
        Ok(format!("Deleted {}", path.display()))
    })
    .await
}

// ============================================================================
// Patches
// ============================================================================

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PatchView {
    pub name: String,
    pub path: String,
    pub valid: bool,
    pub dsf_count: usize,
    pub terrain_count: usize,
    pub texture_count: usize,
    pub regions: Vec<(i32, i32)>,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PatchesView {
    pub directory: String,
    pub exists: bool,
    pub patches: Vec<PatchView>,
}

#[tauri::command]
pub async fn patches_list() -> Result<PatchesView, String> {
    blocking(|| {
        let config = Environment::load()?.config;
        let dir = config
            .patches
            .directory
            .clone()
            .unwrap_or_else(xearthlayer::paths::patches_dir);
        let discovery = PatchDiscovery::new(&dir);
        let patches = if discovery.exists() {
            discovery
                .find_patches()
                .map_err(|e| format!("Failed to scan patches: {e}"))?
        } else {
            Vec::new()
        };
        Ok(PatchesView {
            directory: dir.display().to_string(),
            exists: discovery.exists(),
            patches: patches
                .into_iter()
                .map(|p| PatchView {
                    name: p.name,
                    path: p.path.display().to_string(),
                    valid: p.is_valid,
                    dsf_count: p.dsf_count,
                    terrain_count: p.terrain_count,
                    texture_count: p.texture_count,
                    regions: p.dsf_regions,
                    errors: p.validation_errors.iter().map(|e| e.to_string()).collect(),
                })
                .collect(),
        })
    })
    .await
}

// ============================================================================
// Diagnostics
// ============================================================================

#[tauri::command]
pub async fn diagnostics_report() -> Result<String, String> {
    blocking(|| Ok(SystemReport::collect().to_string())).await
}
