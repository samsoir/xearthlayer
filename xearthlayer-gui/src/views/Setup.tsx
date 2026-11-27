import { useEffect, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { api, Gpu, SetupDetection, SetupForm } from "../api";
import { ActionMessage, Card, Notice, useAction } from "../components/ui";
import { bytes, GB } from "../format";

async function pickFolder(current: string | null): Promise<string | null> {
  const chosen = await open({ directory: true, multiple: false, defaultPath: current ?? undefined });
  return typeof chosen === "string" ? chosen : null;
}

function FolderField({ label, value, onChange, hint }: {
  label: string;
  value: string;
  onChange: (v: string) => void;
  hint?: string;
}) {
  return (
    <label className="field">
      <span>{label}</span>
      <div className="input-with-button">
        <input value={value} onChange={(e) => onChange(e.target.value)} spellCheck={false} />
        <button type="button" onClick={async () => { const p = await pickFolder(value || null); if (p) onChange(p); }}>
          Browse…
        </button>
      </div>
      {hint && <small className="muted">{hint}</small>}
    </label>
  );
}

export function Setup({ onSaved }: { onSaved: () => void }) {
  const [detection, setDetection] = useState<SetupDetection | null>(null);
  const [form, setForm] = useState<SetupForm | null>(null);
  const [gpus, setGpus] = useState<Gpu[] | null>(null);
  const load = useAction();
  const gpuAction = useAction();
  const save = useAction();

  useEffect(() => {
    load.run(api.setupDetect).then((d) => {
      if (d) {
        setDetection(d);
        setForm(d.form);
      }
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  if (!detection || !form) {
    return (
      <div className="page">
        <h1>Setup</h1>
        <ActionMessage message={load.message} />
        {!load.message && <p className="muted">Detecting X-Plane and your hardware…</p>}
      </div>
    );
  }

  const set = <K extends keyof SetupForm>(k: K, v: SetupForm[K]) => setForm({ ...form, [k]: v });
  const provider = detection.providers.find((p) => p.key === form.providerType);
  const hw = detection.hardware;

  return (
    <div className="page">
      <header className="page-header">
        <div>
          <h1>Setup</h1>
          <p className="muted">
            {detection.configExists
              ? "Review your configuration. Saving keeps any setting not shown here."
              : "First-time setup. Pick where X-Plane is and how much space XEarthLayer may use."}
          </p>
        </div>
      </header>

      <Card title="1. X-Plane">
        {detection.xplaneCandidates.length > 1 && (
          <label className="field">
            <span>Detected installations</span>
            <select value={form.sceneryDir ?? ""} onChange={(e) => set("sceneryDir", e.target.value)}>
              {detection.xplaneCandidates.map((c) => (
                <option key={c} value={c}>
                  {c}
                </option>
              ))}
            </select>
          </label>
        )}
        {detection.xplaneCandidates.length === 0 && (
          <Notice kind="warn">X-Plane 12 was not detected. Choose its Custom Scenery folder.</Notice>
        )}
        <FolderField label="Custom Scenery folder" value={form.sceneryDir ?? ""} onChange={(v) => set("sceneryDir", v)} />
        <label className="check">
          <input type="checkbox" checked={form.xworldPro} onChange={(e) => set("xworldPro", e.target.checked)} />
          I use SimHeaven X-World Pro (give it priority over XEarthLayer overlays)
        </label>
      </Card>

      <Card title="2. Packages">
        <FolderField
          label="Package folder"
          value={form.packageDir}
          onChange={(v) => set("packageDir", v)}
          hint="Regional scenery packages are installed here. Choose a drive with plenty of space."
        />
      </Card>

      <Card title="3. Imagery">
        <label className="field">
          <span>Provider</span>
          <select value={form.providerType} onChange={(e) => set("providerType", e.target.value)}>
            {detection.providers.map((p) => (
              <option key={p.key} value={p.key}>
                {p.name}
                {p.credential ? " (needs a key)" : ""}
              </option>
            ))}
          </select>
          {provider && <small className="muted">{provider.summary}</small>}
        </label>
        {provider?.credential === "google" && (
          <label className="field">
            <span>Google Maps API key</span>
            <input
              type="password"
              placeholder={detection.hasGoogleKey ? "Saved — leave empty to keep it" : ""}
              value={form.googleApiKey}
              onChange={(e) => set("googleApiKey", e.target.value)}
            />
          </label>
        )}
        {provider?.credential === "mapbox" && (
          <label className="field">
            <span>MapBox access token</span>
            <input
              type="password"
              placeholder={detection.hasMapboxToken ? "Saved — leave empty to keep it" : ""}
              value={form.mapboxAccessToken}
              onChange={(e) => set("mapboxAccessToken", e.target.value)}
            />
          </label>
        )}
      </Card>

      <Card title="4. Network">
        <label className="field">
          <span>Connections</span>
          <select value={form.networkConnections} onChange={(e) => set("networkConnections", e.target.value)}>
            <option value="max">Max — fastest; some home routers drop the network</option>
            <option value="high">High — recommended</option>
            <option value="low">Low — for routers with small connection tables</option>
          </select>
        </label>
        {form.networkConnections === "max" && (
          <Notice kind="warn">Some home routers drop the whole network when their connection table fills. Use High if that happens.</Notice>
        )}
      </Card>

      <Card title="5. Cache">
        <p className="muted small">
          {hw.cpuCores} CPU cores · {hw.memory} memory · {hw.storage} storage · {bytes(hw.availableBytes)} free
        </p>
        <FolderField label="Cache folder" value={form.cacheDir} onChange={(v) => set("cacheDir", v)} />
        <div className="grid-2">
          <label className="field">
            <span>Disk cache (GB)</span>
            <input
              type="number"
              min={1}
              value={Math.round(form.diskCacheSize / GB)}
              onChange={(e) => set("diskCacheSize", Number(e.target.value) * GB)}
            />
            <small className="muted">Recommended {bytes(hw.recommendedDiskCache)}</small>
          </label>
          <label className="field">
            <span>Memory cache (GB)</span>
            <input
              type="number"
              min={0.25}
              step={0.25}
              value={+(form.memoryCacheSize / GB).toFixed(2)}
              onChange={(e) => set("memoryCacheSize", Math.round(Number(e.target.value) * GB))}
            />
            <small className="muted">Recommended {bytes(hw.recommendedMemoryCache)}</small>
          </label>
        </div>
        <label className="field">
          <span>Share of the disk cache for encoded tiles: {Math.round(form.ddsDiskRatio * 100)}%</span>
          <input
            type="range"
            min={0.1}
            max={0.9}
            step={0.05}
            value={form.ddsDiskRatio}
            onChange={(e) => set("ddsDiskRatio", Number(e.target.value))}
          />
        </label>
      </Card>

      <Card title="6. Texture encoding">
        <label className="field">
          <span>Encoder</span>
          <select
            value={form.textureCompressor === "gpu" ? `gpu:${form.textureGpuDevice}` : form.textureCompressor}
            onChange={(e) => {
              const v = e.target.value;
              if (v.startsWith("gpu:")) setForm({ ...form, textureCompressor: "gpu", textureGpuDevice: v.slice(4) });
              else setForm({ ...form, textureCompressor: v, textureGpuDevice: "integrated" });
            }}
          >
            <option value="ispc">ISPC (CPU) — recommended</option>
            <option value="software">Software (slowest, most compatible)</option>
            {form.textureCompressor === "gpu" && !gpus && (
              <option value={`gpu:${form.textureGpuDevice}`}>GPU ({form.textureGpuDevice})</option>
            )}
            {gpus?.map((g) => (
              <option key={g.configValue} value={`gpu:${g.configValue}`}>
                GPU: {g.name}
              </option>
            ))}
          </select>
        </label>
        {!gpus && (
          <button disabled={gpuAction.busy} onClick={() => gpuAction.run(api.setupGpus).then((g) => g && setGpus(g))}>
            {gpuAction.busy ? "Detecting GPUs (this can take a while)…" : "Detect GPUs"}
          </button>
        )}
        {gpus && gpus.length === 0 && <Notice kind="info">No second GPU was found. ISPC is the right choice.</Notice>}
        {form.textureCompressor === "gpu" && gpus?.[0]?.warning && <Notice kind="warn">{gpus[0].warning}</Notice>}
        <Notice kind="info">Do not choose the GPU X-Plane renders with: it costs frames.</Notice>
        <ActionMessage message={gpuAction.message} />
      </Card>

      <div className="sticky-actions">
        <ActionMessage message={save.message} />
        <button
          className="primary"
          disabled={save.busy}
          onClick={() =>
            save.run(
              () => api.setupSave(form),
              (path) => {
                onSaved();
                return `Saved to ${path}`;
              },
            )
          }
        >
          Save configuration
        </button>
      </div>
    </div>
  );
}
