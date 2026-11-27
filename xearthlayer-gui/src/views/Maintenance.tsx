import { useState } from "react";
import { api } from "../api";
import { ActionMessage, Badge, Card, ConfirmButton, Meter, Notice, Tabs, useAction, usePoll } from "../components/ui";
import { bytes, count } from "../format";

type Tab = "cache" | "index" | "patches" | "diagnostics";

export function Maintenance() {
  const [tab, setTab] = useState<Tab>("cache");
  return (
    <div className="page">
      <header className="page-header">
        <div>
          <h1>Maintenance</h1>
          <p className="muted">Caches, the scenery index, tile patches and diagnostics for bug reports.</p>
        </div>
      </header>
      <Tabs
        tabs={[
          { id: "cache", label: "Disk cache" },
          { id: "index", label: "Scenery index" },
          { id: "patches", label: "Patches" },
          { id: "diagnostics", label: "Diagnostics" },
        ]}
        active={tab}
        onChange={setTab}
      />
      {tab === "cache" && <DiskCache />}
      {tab === "index" && <SceneryIndex />}
      {tab === "patches" && <Patches />}
      {tab === "diagnostics" && <Diagnostics />}
    </div>
  );
}

function DiskCache() {
  const { data, error, refresh } = usePoll(api.cacheStats, 0);
  const action = useAction();
  return (
    <Card
      title="Disk cache"
      actions={
        <>
          <ConfirmButton
            label="Clear cache"
            confirmLabel="Delete every cached tile?"
            disabled={action.busy}
            onConfirm={() => action.run(api.cacheClear, (m) => { refresh(); return m; })}
          />
          <button onClick={refresh}>Refresh</button>
        </>
      }
    >
      {error && <Notice kind="error">{error}</Notice>}
      <ActionMessage message={action.message} />
      {data && (
        <>
          <p className="mono small muted">{data.directory}</p>
          <Meter value={data.bytes} max={data.budgetBytes} />
          <p>
            {count(data.files)} files, {bytes(data.bytes)} of {bytes(data.budgetBytes)}
          </p>
        </>
      )}
    </Card>
  );
}

function SceneryIndex() {
  const { data, error, refresh } = usePoll(api.sceneryIndexStatus, 0);
  const action = useAction();
  const done = (m: string) => {
    refresh();
    return m;
  };
  return (
    <Card
      title="Scenery index"
      actions={
        <>
          <button className="primary" disabled={action.busy} onClick={() => action.run(api.sceneryIndexUpdate, done)}>
            {action.busy ? "Rebuilding…" : "Rebuild"}
          </button>
          <ConfirmButton label="Delete" disabled={action.busy} onConfirm={() => action.run(api.sceneryIndexClear, done)} />
        </>
      }
    >
      <p className="muted small">The index of installed tiles that prefetch uses. XEarthLayer rebuilds it when packages change.</p>
      {error && <Notice kind="error">{error}</Notice>}
      <ActionMessage message={action.message} />
      {data && (
        <>
          <p className="mono small muted">{data.path}</p>
          {!data.exists && <Badge tone="grey">Not built</Badge>}
          {data.exists && !data.valid && <Notice kind="error">Invalid: {data.error}</Notice>}
          {data.valid && (
            <dl className="facts">
              <dt>Tiles</dt>
              <dd>
                {count(data.totalTiles)} ({count(data.totalTiles - data.seaTiles)} land, {count(data.seaTiles)} sea)
              </dd>
              <dt>Coverage</dt>
              <dd>
                {count(data.totalTiles)} of {count(data.expectedTiles)} .ter files
                {data.totalTiles < data.expectedTiles && <span className="warn-text"> — {count(data.expectedTiles - data.totalTiles)} produced no tile</span>}
              </dd>
              <dt>File size</dt>
              <dd>{bytes(data.fileBytes)}</dd>
              <dt>Packages</dt>
              <dd>{data.packages.map((p) => `${p.name} (${count(p.terrainFiles)})`).join(", ")}</dd>
            </dl>
          )}
        </>
      )}
    </Card>
  );
}

function Patches() {
  const { data, error, refresh } = usePoll(api.patches, 0);
  return (
    <Card title="Tile patches" actions={<button onClick={refresh}>Refresh</button>}>
      <p className="muted small">
        Ortho4XP tiles with custom mesh from airport add-ons. XEarthLayer generates their textures. Put each tile folder in
        the patches folder.
      </p>
      {error && <Notice kind="error">{error}</Notice>}
      {data && <p className="mono small muted">{data.directory}</p>}
      {data && !data.exists && <Notice kind="info">The patches folder does not exist yet.</Notice>}
      {data && data.exists && data.patches.length === 0 && <p className="muted">No patches found.</p>}
      {data && data.patches.length > 0 && (
        <table>
          <thead>
            <tr>
              <th>Patch</th>
              <th>Status</th>
              <th className="num">DSF</th>
              <th className="num">Terrain</th>
              <th className="num">Textures</th>
              <th>Regions</th>
            </tr>
          </thead>
          <tbody>
            {data.patches.map((p) => (
              <tr key={p.path}>
                <td className="strong">{p.name}</td>
                <td>
                  {p.valid ? <Badge tone="green">Valid</Badge> : <Badge tone="red">Invalid</Badge>}
                  {p.errors.map((e) => (
                    <div key={e} className="error-text small">
                      {e}
                    </div>
                  ))}
                </td>
                <td className="num">{p.dsfCount}</td>
                <td className="num">{p.terrainCount}</td>
                <td className="num">{p.textureCount}</td>
                <td className="mono small">{p.regions.map(([lat, lon]) => `${lat >= 0 ? "+" : ""}${lat}${lon >= 0 ? "+" : ""}${lon}`).join(" ")}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </Card>
  );
}

function Diagnostics() {
  const action = useAction();
  const [report, setReport] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);
  return (
    <Card
      title="Diagnostics"
      actions={
        <>
          <button className="primary" disabled={action.busy} onClick={() => action.run(api.diagnostics).then((r) => r && setReport(r))}>
            {report ? "Collect again" : "Collect report"}
          </button>
          {report && (
            <button
              onClick={async () => {
                await navigator.clipboard.writeText(report);
                setCopied(true);
                window.setTimeout(() => setCopied(false), 2000);
              }}
            >
              {copied ? "Copied" : "Copy"}
            </button>
          )}
        </>
      }
    >
      <p className="muted small">System information to paste into a bug report. Review it before sharing.</p>
      <ActionMessage message={action.message} />
      {report && <pre className="report">{report}</pre>}
    </Card>
  );
}
