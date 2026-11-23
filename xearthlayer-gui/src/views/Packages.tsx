import { ReactElement, useMemo, useState } from "react";
import { api, Job, LibraryRow, PackageRef } from "../api";
import { ActionMessage, Badge, Card, ConfirmButton, Meter, Notice, Tabs, useAction, usePoll } from "../components/ui";
import { bytes } from "../format";

type Tab = "installed" | "library" | "progress";

export function Packages() {
  const [tab, setTab] = useState<Tab>("installed");
  const { data: jobs } = usePoll(api.jobs, 750);
  const running = jobs?.jobs.filter((j) => !j.finished).length ?? 0;

  return (
    <div className="page">
      <header className="page-header">
        <div>
          <h1>Packages</h1>
          <p className="muted">Regional scenery packages: install, update and remove.</p>
        </div>
      </header>
      <Tabs
        tabs={[
          { id: "installed", label: "Installed" },
          { id: "library", label: "Library" },
          { id: "progress", label: running > 0 ? `Progress (${running})` : "Progress" },
        ]}
        active={tab}
        onChange={setTab}
      />
      {tab === "installed" && <Installed onStarted={() => setTab("progress")} />}
      {tab === "library" && <Library onStarted={() => setTab("progress")} />}
      {tab === "progress" && <Progress />}
    </div>
  );
}

const key = (p: PackageRef) => `${p.region}/${p.packageType}`;

function Installed({ onStarted }: { onStarted: () => void }) {
  const { data, error, refresh } = usePoll(api.installed, 0);
  const action = useAction();

  const remove = (p: PackageRef) =>
    action.run(async () => {
      await api.remove([p]);
      onStarted();
    });

  return (
    <Card
      title="Installed packages"
      actions={
        <>
          <button onClick={() => action.run(api.rebuildOverlays, (notes) => notes.join(" "))}>Rebuild overlays</button>
          <button onClick={refresh}>Refresh</button>
        </>
      }
    >
      {error && <Notice kind="error">{error}</Notice>}
      <ActionMessage message={action.message} />
      {data && <p className="muted small mono">{data.installLocation}</p>}
      {data && data.packages.length === 0 && <p className="muted">No packages installed yet. Pick some from the Library tab.</p>}
      {data && data.packages.length > 0 && (
        <table>
          <thead>
            <tr>
              <th>Region</th>
              <th>Type</th>
              <th>Version</th>
              <th className="num">Size</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {data.packages.map((p) => (
              <tr key={key(p)}>
                <td className="strong">{p.region}</td>
                <td>{p.packageType}</td>
                <td className="mono">{p.version}</td>
                <td className="num">{bytes(p.sizeBytes)}</td>
                <td className="row-actions">
                  <ConfirmButton label="Remove" confirmLabel="Confirm remove" onConfirm={() => remove(p)} disabled={action.busy} />
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </Card>
  );
}

const STATUS_BADGE: Record<LibraryRow["status"], ReactElement> = {
  notInstalled: <Badge tone="grey">Not installed</Badge>,
  upToDate: <Badge tone="green">Installed</Badge>,
  update: <Badge tone="amber">Update available</Badge>,
};

function Library({ onStarted }: { onStarted: () => void }) {
  const { data, error, refresh } = usePoll(api.library, 0);
  const action = useAction();
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [filter, setFilter] = useState("");
  const [type, setType] = useState<"all" | "ortho" | "overlay">("ortho");

  const rows = useMemo(
    () =>
      (data ?? []).filter(
        (r) => (type === "all" || r.packageType === type) && r.region.includes(filter.trim().toUpperCase()),
      ),
    [data, filter, type],
  );
  const updates = (data ?? []).filter((r) => r.status === "update").length;

  const toggle = (r: LibraryRow) => {
    const next = new Set(selected);
    if (next.has(key(r))) next.delete(key(r));
    else next.add(key(r));
    setSelected(next);
  };

  const install = () =>
    action.run(async () => {
      const refs = (data ?? []).filter((r) => selected.has(key(r))).map(({ region, packageType }) => ({ region, packageType }));
      await api.install(refs);
      setSelected(new Set());
      onStarted();
    });

  return (
    <Card
      title="Library"
      actions={
        <>
          {updates > 0 && (
            <button onClick={() => action.run(async () => { await api.updateAll(); onStarted(); })} disabled={action.busy}>
              Update all ({updates})
            </button>
          )}
          <button className="primary" disabled={selected.size === 0 || action.busy} onClick={install}>
            Install selected ({selected.size})
          </button>
          <button onClick={refresh}>Refresh</button>
        </>
      }
    >
      <div className="form-row">
        <input placeholder="Filter by region, e.g. EU or NA-CANADA" value={filter} onChange={(e) => setFilter(e.target.value)} />
        <select value={type} onChange={(e) => setType(e.target.value as typeof type)}>
          <option value="ortho">Ortho</option>
          <option value="overlay">Overlay</option>
          <option value="all">All types</option>
        </select>
      </div>
      {error && <Notice kind="error">{error}</Notice>}
      <ActionMessage message={action.message} />
      {!data && !error && <p className="muted">Fetching the library…</p>}
      {data && (
        <table>
          <thead>
            <tr>
              <th />
              <th>Region</th>
              <th>Type</th>
              <th>Version</th>
              <th>Status</th>
            </tr>
          </thead>
          <tbody>
            {rows.map((r) => (
              <tr key={key(r)} className="clickable" onClick={() => toggle(r)}>
                <td>
                  <input type="checkbox" checked={selected.has(key(r))} onChange={() => toggle(r)} onClick={(e) => e.stopPropagation()} />
                </td>
                <td className="strong">{r.region}</td>
                <td>{r.packageType}</td>
                <td className="mono">
                  {r.status === "update" && r.installedVersion ? `${r.installedVersion} → ${r.version}` : r.version}
                </td>
                <td>{STATUS_BADGE[r.status]}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </Card>
  );
}

function Progress() {
  const { data } = usePoll(api.jobs, 500);
  const action = useAction();
  if (!data) return null;
  return (
    <Card title="Progress" actions={<button onClick={() => api.clearJobs()}>Clear finished</button>}>
      <ActionMessage message={action.message} />
      {data.notes.map((n) => (
        <Notice key={n} kind="info">
          {n}
        </Notice>
      ))}
      {data.jobs.length === 0 && <p className="muted">Nothing has run yet.</p>}
      <div className="jobs">
        {data.jobs.map((job) => (
          <JobRow key={job.id} job={job} onCancel={() => action.run(() => api.cancelJob(job.id), (m) => m)} />
        ))}
      </div>
    </Card>
  );
}

function JobRow({ job, onCancel }: { job: Job; onCancel: () => void }) {
  const tone = job.finished ? (job.succeeded ? "green" : "red") : "blue";
  return (
    <div className="job">
      <div className="job-title">
        <span className="strong">
          {job.kind === "install" ? "Install" : "Remove"} {job.region} {job.packageType}
          {job.version && <span className="mono muted"> {job.version}</span>}
        </span>
        <Badge tone={tone}>{job.stage}</Badge>
        {!job.finished && (
          <button className="small-button" onClick={onCancel}>
            Cancel
          </button>
        )}
      </div>
      {job.stage === "Downloading" && (
        <>
          <Meter value={job.bytesDone} max={job.bytesTotal ?? 0} />
          <div className="muted small mono">
            {bytes(job.bytesDone)} / {bytes(job.bytesTotal)}
          </div>
        </>
      )}
      {job.message && <div className={job.finished && !job.succeeded ? "error-text small" : "muted small"}>{job.message}</div>}
    </div>
  );
}
