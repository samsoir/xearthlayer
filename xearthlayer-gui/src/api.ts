// Typed wrappers over the Tauri commands in src-tauri/src. Every type here
// mirrors a serde struct there (camelCase).
import { invoke } from "@tauri-apps/api/core";

export type Phase = "idle" | "starting" | "running" | "stopping" | "failed";

export interface Telemetry {
  uptimeSecs: number;
  fuseTilesServed: number;
  fuseRequestsActive: number;
  fuseRequestsWaiting: number;
  jobsSubmitted: number;
  jobsCompleted: number;
  jobsFailed: number;
  jobsTimedOut: number;
  jobsActive: number;
  jobsCoalesced: number;
  jobsPerSecond: number;
  chunksDownloaded: number;
  chunksFailed: number;
  chunksRetried: number;
  bytesDownloaded: number;
  downloadsActive: number;
  bytesPerSecond: number;
  peakBytesPerSecond: number;
  encodesCompleted: number;
  encodesActive: number;
  memoryHitRate: number;
  ddsDiskHitRate: number;
  chunkDiskHitRate: number;
  memoryCacheBytes: number;
  ddsDiskCacheBytes: number;
  chunkDiskCacheBytes: number;
}

export interface ServiceStatus {
  phase: Phase;
  error: string | null;
  startup: { step: string; detail: string | null; done: number; total: number } | null;
  mount: { mountpoint: string; sourceCount: number; patches: string[]; packages: string[] } | null;
  features: {
    provider: string;
    encoder: string;
    encoderWarning: string | null;
    nightLighting: string;
    prefetchEnabled: boolean;
    memoryCacheMax: number;
    ddsDiskCacheMax: number;
    chunkDiskCacheMax: number;
  } | null;
  telemetry: Telemetry | null;
  live: {
    prefetchState: string;
    telemetryConnected: boolean;
    position: [number, number] | null;
    positionSource: string | null;
  } | null;
  prewarm: {
    icao: string;
    airport: string;
    completed: number;
    total: number;
    cacheHits: number;
    complete: boolean;
    cancelled: boolean;
  } | null;
  updateAvailable: string | null;
  lastSession: Telemetry | null;
}

export interface RunOptions {
  airport?: string;
  noPrefetch?: boolean;
  noCache?: boolean;
}

export interface AppInfo {
  version: string;
  configPath: string;
  configExists: boolean;
  installLocation: string;
  customScenery: string | null;
  logFile: string;
  platform: string;
}

export interface InstalledPackage {
  region: string;
  packageType: string;
  version: string;
  path: string;
  sizeBytes: number;
}

export type LibraryStatus = "notInstalled" | "upToDate" | "update";

export interface LibraryRow {
  region: string;
  packageType: string;
  version: string;
  status: LibraryStatus;
  installedVersion: string | null;
}

export interface PackageRef {
  region: string;
  packageType: string;
}

export interface Job {
  id: number;
  kind: "install" | "remove";
  region: string;
  packageType: string;
  version: string | null;
  stage: string;
  bytesDone: number;
  bytesTotal: number | null;
  finished: boolean;
  succeeded: boolean;
  message: string | null;
}

export interface Jobs {
  busy: boolean;
  jobs: Job[];
  notes: string[];
}

export interface Setting {
  name: string;
  section: string;
  key: string;
  value: string;
  isSet: boolean;
  sensitive: boolean;
  help: string[];
}

export interface UpgradeReport {
  exists: boolean;
  needsUpgrade: boolean;
  missing: string[];
  deprecated: string[];
  rewritten: string[];
  unknown: string[];
  applied: boolean;
  backup: string | null;
}

export interface SetupForm {
  sceneryDir: string | null;
  packageDir: string;
  cacheDir: string;
  memoryCacheSize: number;
  diskCacheSize: number;
  ddsDiskRatio: number;
  textureCompressor: string;
  textureGpuDevice: string;
  providerType: string;
  googleApiKey: string;
  mapboxAccessToken: string;
  xworldPro: boolean;
  networkConnections: string;
}

export interface SetupDetection {
  form: SetupForm;
  xplaneCandidates: string[];
  providers: { key: string; name: string; summary: string; credential: "google" | "mapbox" | null }[];
  hardware: {
    cpuCores: number;
    memory: string;
    storage: string;
    availableBytes: number;
    recommendedMemoryCache: number;
    recommendedDiskCache: number;
  };
  hasGoogleKey: boolean;
  hasMapboxToken: boolean;
  configExists: boolean;
}

export interface Gpu {
  name: string;
  configValue: string;
  warning: string | null;
}

export interface CacheStats {
  directory: string;
  files: number;
  bytes: number;
  budgetBytes: number;
}

export interface SceneryIndexStatus {
  path: string;
  exists: boolean;
  valid: boolean;
  error: string | null;
  version: string | null;
  packages: { name: string; terrainFiles: number }[];
  totalTiles: number;
  seaTiles: number;
  fileBytes: number;
  expectedTiles: number;
}

export interface Patches {
  directory: string;
  exists: boolean;
  patches: {
    name: string;
    path: string;
    valid: boolean;
    dsfCount: number;
    terrainCount: number;
    textureCount: number;
    regions: [number, number][];
    errors: string[];
  }[];
}

export const api = {
  appInfo: () => invoke<AppInfo>("app_info"),

  serviceStart: (options: RunOptions) => invoke<void>("service_start", { options }),
  serviceStop: () => invoke<void>("service_stop"),
  serviceStatus: () => invoke<ServiceStatus>("service_status"),
  cancelPrewarm: () => invoke<void>("service_cancel_prewarm"),

  installed: () => invoke<{ installLocation: string; packages: InstalledPackage[] }>("packages_installed"),
  library: () => invoke<LibraryRow[]>("packages_library"),
  install: (packages: PackageRef[]) => invoke<void>("packages_install", { packages }),
  updateAll: () => invoke<number>("packages_update_all"),
  remove: (packages: PackageRef[]) => invoke<void>("packages_remove", { packages }),
  jobs: () => invoke<Jobs>("package_jobs"),
  clearJobs: () => invoke<void>("package_jobs_clear"),
  cancelJob: (id: number) => invoke<string>("package_cancel", { id }),
  rebuildOverlays: () => invoke<string[]>("packages_rebuild_overlays"),

  configList: () => invoke<Setting[]>("config_list"),
  configSet: (key: string, value: string) => invoke<string>("config_set", { key, value }),
  configUpgrade: (dryRun: boolean) => invoke<UpgradeReport>("config_upgrade", { dryRun }),
  configInit: () => invoke<string>("config_init"),

  setupDetect: () => invoke<SetupDetection>("setup_detect"),
  setupGpus: () => invoke<Gpu[]>("setup_gpus"),
  setupSave: (form: SetupForm) => invoke<string>("setup_save", { form }),

  cacheStats: () => invoke<CacheStats>("cache_stats"),
  cacheClear: () => invoke<string>("cache_clear"),
  sceneryIndexStatus: () => invoke<SceneryIndexStatus>("scenery_index_status"),
  sceneryIndexUpdate: () => invoke<string>("scenery_index_update"),
  sceneryIndexClear: () => invoke<string>("scenery_index_clear"),
  patches: () => invoke<Patches>("patches_list"),
  diagnostics: () => invoke<string>("diagnostics_report"),
};

/// The message of a rejected command: commands reject with a plain string.
export function errorText(e: unknown): string {
  if (typeof e === "string") return e;
  if (e instanceof Error) return e.message;
  return String(e);
}
