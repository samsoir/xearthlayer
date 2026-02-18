//! GeoIndex — In-memory geospatial reference database.
//!
//! A type-keyed, region-indexed spatial store. Each "layer" is an independent
//! collection of typed data indexed by 1°×1° DSF regions. Layers are created
//! implicitly on first write.
//!
//! # Thread Safety
//!
//! - Layer-level access protected by `RwLock` (rare writes for layer creation)
//! - Region-level access via `DashMap` (concurrent reads, per-shard write locks)
//!
//! # ACID Properties
//!
//! - **Atomicity**: `populate()` builds data outside lock, swaps atomically
//! - **Consistency**: Readers see either old or new state, never partial
//! - **Isolation**: Multiple readers access concurrently via `RwLock` read guards
//! - **Durability**: N/A (in-memory; serialization can be added later)

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::RwLock;

use dashmap::DashMap;

use super::layers::GeoLayer;
use super::region::DsfRegion;

/// Internal storage for a single typed layer.
///
/// Wraps a `DashMap` for concurrent region-level access.
struct LayerStore<T: GeoLayer> {
    data: DashMap<DsfRegion, T>,
}

/// In-memory geospatial reference database.
///
/// A type-keyed, region-indexed spatial store. Each "layer" is an
/// independent collection of typed data indexed by 1°×1° DSF regions.
/// Layers are created implicitly on first write.
///
/// # Thread Safety
///
/// - Layer-level access protected by `RwLock` (rare writes for layer creation)
/// - Region-level access via `DashMap` (concurrent reads, per-shard write locks)
///
/// # ACID Properties
///
/// - **Atomicity**: `populate()` builds data outside lock, swaps atomically
/// - **Consistency**: Readers see either old or new state, never partial
/// - **Isolation**: Multiple readers access concurrently via `RwLock` read guards
/// - **Durability**: N/A (in-memory; serialization can be added later)
pub struct GeoIndex {
    layers: RwLock<HashMap<TypeId, Box<dyn Any + Send + Sync>>>,
}

impl GeoIndex {
    /// Create a new empty GeoIndex.
    pub fn new() -> Self {
        Self {
            layers: RwLock::new(HashMap::new()),
        }
    }

    /// Check if a region has data of the given layer type.
    ///
    /// Returns `false` if the layer doesn't exist or the region has no entry.
    pub fn contains<T: GeoLayer>(&self, region: &DsfRegion) -> bool {
        let layers = self.layers.read().expect("GeoIndex lock poisoned");
        match layers.get(&TypeId::of::<T>()) {
            Some(boxed) => {
                let store = boxed
                    .downcast_ref::<LayerStore<T>>()
                    .expect("type mismatch");
                store.data.contains_key(region)
            }
            None => false,
        }
    }

    /// Get a cloned value for a region in the given layer type.
    ///
    /// Returns `None` if the layer doesn't exist or the region has no entry.
    pub fn get<T: GeoLayer>(&self, region: &DsfRegion) -> Option<T> {
        let layers = self.layers.read().expect("GeoIndex lock poisoned");
        let boxed = layers.get(&TypeId::of::<T>())?;
        let store = boxed
            .downcast_ref::<LayerStore<T>>()
            .expect("type mismatch");
        store.data.get(region).map(|entry| entry.value().clone())
    }

    /// Insert or update a single region entry.
    ///
    /// Creates the layer if it doesn't exist yet.
    pub fn insert<T: GeoLayer>(&self, region: DsfRegion, value: T) {
        // Try with read lock first (common case: layer exists)
        {
            let layers = self.layers.read().expect("GeoIndex lock poisoned");
            if let Some(boxed) = layers.get(&TypeId::of::<T>()) {
                let store = boxed
                    .downcast_ref::<LayerStore<T>>()
                    .expect("type mismatch");
                store.data.insert(region, value);
                return;
            }
        }
        // Layer doesn't exist — create with write lock
        let mut layers = self.layers.write().expect("GeoIndex lock poisoned");
        // Double-check after acquiring write lock
        if let Some(boxed) = layers.get(&TypeId::of::<T>()) {
            let store = boxed
                .downcast_ref::<LayerStore<T>>()
                .expect("type mismatch");
            store.data.insert(region, value);
            return;
        }
        let store = LayerStore {
            data: DashMap::new(),
        };
        store.data.insert(region, value);
        layers.insert(TypeId::of::<T>(), Box::new(store));
    }

    /// Remove a region entry from a layer.
    ///
    /// Returns the removed value, or `None` if it didn't exist.
    pub fn remove<T: GeoLayer>(&self, region: &DsfRegion) -> Option<T> {
        let layers = self.layers.read().expect("GeoIndex lock poisoned");
        let boxed = layers.get(&TypeId::of::<T>())?;
        let store = boxed
            .downcast_ref::<LayerStore<T>>()
            .expect("type mismatch");
        store.data.remove(region).map(|(_, v)| v)
    }

    /// Atomic bulk population — replaces the entire layer.
    ///
    /// Builds the new `DashMap` outside the lock (readers not blocked),
    /// then swaps atomically with a brief write lock.
    pub fn populate<T: GeoLayer>(&self, entries: impl IntoIterator<Item = (DsfRegion, T)>) {
        // Build new store outside the lock
        let new_data = DashMap::new();
        for (region, value) in entries {
            new_data.insert(region, value);
        }
        let new_store = LayerStore { data: new_data };

        // Swap atomically
        let mut layers = self.layers.write().expect("GeoIndex lock poisoned");
        layers.insert(TypeId::of::<T>(), Box::new(new_store));
    }

    /// Number of regions in a layer.
    ///
    /// Returns 0 if the layer doesn't exist.
    pub fn count<T: GeoLayer>(&self) -> usize {
        let layers = self.layers.read().expect("GeoIndex lock poisoned");
        match layers.get(&TypeId::of::<T>()) {
            Some(boxed) => {
                let store = boxed
                    .downcast_ref::<LayerStore<T>>()
                    .expect("type mismatch");
                store.data.len()
            }
            None => 0,
        }
    }

    /// Get all regions in a layer (copied).
    ///
    /// Returns an empty vec if the layer doesn't exist.
    pub fn regions<T: GeoLayer>(&self) -> Vec<DsfRegion> {
        let layers = self.layers.read().expect("GeoIndex lock poisoned");
        match layers.get(&TypeId::of::<T>()) {
            Some(boxed) => {
                let store = boxed
                    .downcast_ref::<LayerStore<T>>()
                    .expect("type mismatch");
                store.data.iter().map(|entry| *entry.key()).collect()
            }
            None => Vec::new(),
        }
    }

    /// Clear all entries in a layer.
    ///
    /// The layer itself remains (as an empty store).
    pub fn clear<T: GeoLayer>(&self) {
        let mut layers = self.layers.write().expect("GeoIndex lock poisoned");
        layers.remove(&TypeId::of::<T>());
    }
}

impl Default for GeoIndex {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: GeoIndex is Send + Sync because:
// - RwLock<HashMap<TypeId, Box<dyn Any + Send + Sync>>> is Send + Sync
// - All layer data is constrained to Send + Sync via GeoLayer trait bound
unsafe impl Send for GeoIndex {}
unsafe impl Sync for GeoIndex {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo_index::PatchCoverage;

    // A second layer type for multi-layer tests.
    #[derive(Debug, Clone)]
    struct TestMarker;
    impl GeoLayer for TestMarker {}

    // =========================================================================
    // Basic CRUD
    // =========================================================================

    #[test]
    fn test_new_empty() {
        let index = GeoIndex::new();
        assert_eq!(index.count::<PatchCoverage>(), 0);
        assert!(index.regions::<PatchCoverage>().is_empty());
    }

    #[test]
    fn test_insert_and_contains() {
        let index = GeoIndex::new();
        let region = DsfRegion::new(43, 6);

        index.insert::<PatchCoverage>(
            region,
            PatchCoverage {
                patch_name: "Nice".to_string(),
            },
        );

        assert!(index.contains::<PatchCoverage>(&region));
    }

    #[test]
    fn test_contains_missing_returns_false() {
        let index = GeoIndex::new();
        let region = DsfRegion::new(43, 6);

        assert!(!index.contains::<PatchCoverage>(&region));
    }

    #[test]
    fn test_contains_wrong_layer_returns_false() {
        let index = GeoIndex::new();
        let region = DsfRegion::new(43, 6);

        index.insert::<PatchCoverage>(
            region,
            PatchCoverage {
                patch_name: "Nice".to_string(),
            },
        );

        // PatchCoverage exists, but TestMarker does not
        assert!(!index.contains::<TestMarker>(&region));
    }

    #[test]
    fn test_get_returns_cloned_value() {
        let index = GeoIndex::new();
        let region = DsfRegion::new(33, -119);

        index.insert::<PatchCoverage>(
            region,
            PatchCoverage {
                patch_name: "LA Patch".to_string(),
            },
        );

        let result = index.get::<PatchCoverage>(&region);
        assert!(result.is_some());
        assert_eq!(result.unwrap().patch_name, "LA Patch");
    }

    #[test]
    fn test_get_missing_returns_none() {
        let index = GeoIndex::new();
        let region = DsfRegion::new(43, 6);

        assert!(index.get::<PatchCoverage>(&region).is_none());
    }

    #[test]
    fn test_remove() {
        let index = GeoIndex::new();
        let region = DsfRegion::new(43, 6);

        index.insert::<PatchCoverage>(
            region,
            PatchCoverage {
                patch_name: "Nice".to_string(),
            },
        );
        assert!(index.contains::<PatchCoverage>(&region));

        let removed = index.remove::<PatchCoverage>(&region);
        assert!(removed.is_some());
        assert_eq!(removed.unwrap().patch_name, "Nice");
        assert!(!index.contains::<PatchCoverage>(&region));
    }

    #[test]
    fn test_remove_missing_returns_none() {
        let index = GeoIndex::new();
        let region = DsfRegion::new(43, 6);

        assert!(index.remove::<PatchCoverage>(&region).is_none());
    }

    // =========================================================================
    // Bulk population (atomicity)
    // =========================================================================

    #[test]
    fn test_populate_atomic_swap() {
        let index = GeoIndex::new();

        let entries = vec![
            (
                DsfRegion::new(43, 6),
                PatchCoverage {
                    patch_name: "A".to_string(),
                },
            ),
            (
                DsfRegion::new(44, 7),
                PatchCoverage {
                    patch_name: "B".to_string(),
                },
            ),
        ];

        index.populate(entries);

        assert_eq!(index.count::<PatchCoverage>(), 2);
        assert!(index.contains::<PatchCoverage>(&DsfRegion::new(43, 6)));
        assert!(index.contains::<PatchCoverage>(&DsfRegion::new(44, 7)));
    }

    #[test]
    fn test_populate_replaces_previous() {
        let index = GeoIndex::new();

        // First population
        index.populate(vec![(
            DsfRegion::new(43, 6),
            PatchCoverage {
                patch_name: "Old".to_string(),
            },
        )]);
        assert_eq!(index.count::<PatchCoverage>(), 1);

        // Second population — replaces entirely
        index.populate(vec![
            (
                DsfRegion::new(50, 10),
                PatchCoverage {
                    patch_name: "New1".to_string(),
                },
            ),
            (
                DsfRegion::new(51, 11),
                PatchCoverage {
                    patch_name: "New2".to_string(),
                },
            ),
        ]);

        assert_eq!(index.count::<PatchCoverage>(), 2);
        assert!(!index.contains::<PatchCoverage>(&DsfRegion::new(43, 6)));
        assert!(index.contains::<PatchCoverage>(&DsfRegion::new(50, 10)));
        assert!(index.contains::<PatchCoverage>(&DsfRegion::new(51, 11)));
    }

    // =========================================================================
    // Count and regions accessors
    // =========================================================================

    #[test]
    fn test_count_and_regions() {
        let index = GeoIndex::new();

        index.insert::<PatchCoverage>(
            DsfRegion::new(43, 6),
            PatchCoverage {
                patch_name: "A".to_string(),
            },
        );
        index.insert::<PatchCoverage>(
            DsfRegion::new(44, 7),
            PatchCoverage {
                patch_name: "B".to_string(),
            },
        );

        assert_eq!(index.count::<PatchCoverage>(), 2);

        let regions = index.regions::<PatchCoverage>();
        assert_eq!(regions.len(), 2);
        assert!(regions.contains(&DsfRegion::new(43, 6)));
        assert!(regions.contains(&DsfRegion::new(44, 7)));
    }

    #[test]
    fn test_count_empty_layer() {
        let index = GeoIndex::new();
        assert_eq!(index.count::<PatchCoverage>(), 0);
        assert_eq!(index.count::<TestMarker>(), 0);
    }

    // =========================================================================
    // Clear
    // =========================================================================

    #[test]
    fn test_clear_layer() {
        let index = GeoIndex::new();

        index.insert::<PatchCoverage>(
            DsfRegion::new(43, 6),
            PatchCoverage {
                patch_name: "A".to_string(),
            },
        );
        assert_eq!(index.count::<PatchCoverage>(), 1);

        index.clear::<PatchCoverage>();
        assert_eq!(index.count::<PatchCoverage>(), 0);
        assert!(!index.contains::<PatchCoverage>(&DsfRegion::new(43, 6)));
    }

    #[test]
    fn test_clear_nonexistent_layer_is_noop() {
        let index = GeoIndex::new();
        index.clear::<PatchCoverage>(); // Should not panic
        assert_eq!(index.count::<PatchCoverage>(), 0);
    }

    // =========================================================================
    // Multiple layer types
    // =========================================================================

    #[test]
    fn test_multiple_layer_types() {
        let index = GeoIndex::new();
        let region = DsfRegion::new(43, 6);

        index.insert::<PatchCoverage>(
            region,
            PatchCoverage {
                patch_name: "Nice".to_string(),
            },
        );
        index.insert::<TestMarker>(region, TestMarker);

        // Both layers exist independently
        assert!(index.contains::<PatchCoverage>(&region));
        assert!(index.contains::<TestMarker>(&region));
        assert_eq!(index.count::<PatchCoverage>(), 1);
        assert_eq!(index.count::<TestMarker>(), 1);

        // Clearing one doesn't affect the other
        index.clear::<PatchCoverage>();
        assert!(!index.contains::<PatchCoverage>(&region));
        assert!(index.contains::<TestMarker>(&region));
    }

    // =========================================================================
    // Concurrency
    // =========================================================================

    #[test]
    fn test_concurrent_reads() {
        use std::sync::Arc;
        use std::thread;

        let index = Arc::new(GeoIndex::new());

        // Pre-populate
        for lat in 0..100 {
            index.insert::<PatchCoverage>(
                DsfRegion::new(lat, 0),
                PatchCoverage {
                    patch_name: format!("patch_{}", lat),
                },
            );
        }

        // Spawn readers
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let idx = Arc::clone(&index);
                thread::spawn(move || {
                    for lat in 0..100 {
                        assert!(idx.contains::<PatchCoverage>(&DsfRegion::new(lat, 0)));
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().expect("reader thread panicked");
        }
    }

    #[test]
    fn test_concurrent_reads_and_writes() {
        use std::sync::Arc;
        use std::thread;

        let index = Arc::new(GeoIndex::new());

        // Writer thread
        let writer_index = Arc::clone(&index);
        let writer = thread::spawn(move || {
            for lat in 0..50 {
                writer_index.insert::<PatchCoverage>(
                    DsfRegion::new(lat, 0),
                    PatchCoverage {
                        patch_name: format!("patch_{}", lat),
                    },
                );
            }
        });

        // Reader threads (may see partial data — that's fine)
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let idx = Arc::clone(&index);
                thread::spawn(move || {
                    let mut found = 0;
                    for lat in 0..50 {
                        if idx.contains::<PatchCoverage>(&DsfRegion::new(lat, 0)) {
                            found += 1;
                        }
                    }
                    found
                })
            })
            .collect();

        writer.join().expect("writer thread panicked");
        for h in handles {
            h.join().expect("reader thread panicked");
        }

        // After writer finishes, all entries should exist
        assert_eq!(index.count::<PatchCoverage>(), 50);
    }

    // =========================================================================
    // Default trait
    // =========================================================================

    #[test]
    fn test_default() {
        let index = GeoIndex::default();
        assert_eq!(index.count::<PatchCoverage>(), 0);
    }

    // =========================================================================
    // Insert overwrites existing value
    // =========================================================================

    #[test]
    fn test_insert_overwrites() {
        let index = GeoIndex::new();
        let region = DsfRegion::new(43, 6);

        index.insert::<PatchCoverage>(
            region,
            PatchCoverage {
                patch_name: "Old".to_string(),
            },
        );
        index.insert::<PatchCoverage>(
            region,
            PatchCoverage {
                patch_name: "New".to_string(),
            },
        );

        assert_eq!(index.count::<PatchCoverage>(), 1);
        assert_eq!(
            index.get::<PatchCoverage>(&region).unwrap().patch_name,
            "New"
        );
    }
}
