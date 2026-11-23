import { useState } from "react";
import { api, ServiceStatus, Telemetry } from "../api";
import { ActionMessage, Badge, Card, Meter, Notice, Stat, useAction, usePoll } from "../components/ui";
import { bytes, count, duration, percent, rate } from "../format";

const PHASE_LABEL: Record<ServiceStatus["phase"], string> = {
  idle: "Stopped",
  starting: "Starting",
  running: "Running",
  stopping: "Stopping",
  failed: "Failed",
};

const PHASE_TONE = {
  idle: "grey",
  starting: "amber",
  running: "green",
  stopping: "amber",
  failed: "red",
} as const;

export function Dashboard() {
  const { data: status } = usePoll(api.serviceStatus, 1000);
  const action = useAction();
  const [airport, setAirport] = useState("");
  const [noPrefetch, setNoPrefetch] = useState(false);
  const [noCache, setNoCache] = useState(false);

  if (!status) return <p className="muted">Loading…</p>;
  const active = status.phase === "starting" || status.phase === "running" || status.phase === "stopping";

  return (
    <div className="page">
      <header className="page-header">
        <div>
          <h1>Dashboard</h1>
          <p className="muted">Stream satellite imagery to X-Plane while you fly.</p>
        </div>
        <Badge tone={PHASE_TONE[status.phase]}>{PHASE_LABEL[status.phase]}</Badge>
      </header>

      {status.updateAvailable && <Notice kind="info">{status.updateAvailable}</Notice>}

      <Card
        title="Service"
        actions={
          active ? (
            <button className="danger" disabled={status.phase === "stopping"} onClick={() => action.run(api.serviceStop)}>
              {status.phase === "stopping" ? "Stopping…" : "Stop"}
            </button>
          ) : (
            <button
              className="primary"
              disabled={action.busy}
              onClick={() =>
                action.run(() => api.serviceStart({ airport: airport.trim() || undefined, noPrefetch, noCache }))
              }
            >
              Start XEarthLayer
            </button>
          )
        }
      >
        {!active && (
          <div className="form-row">
            <label>
              Pre-warm around airport
              <input
                placeholder="ICAO, e.g. KSEA"
                value={airport}
                maxLength={8}
                onChange={(e) => setAirport(e.target.value.toUpperCase())}
              />
            </label>
            <label className="check">
              <input type="checkbox" checked={noPrefetch} onChange={(e) => setNoPrefetch(e.target.checked)} />
              Disable prefetch
            </label>
            <label className="check">
              <input type="checkbox" checked={noCache} onChange={(e) => setNoCache(e.target.checked)} />
              Disable cache
            </label>
          </div>
        )}
        <ActionMessage message={action.message} />
        {status.error && <Notice kind={status.phase === "failed" ? "error" : "warn"}>{status.error}</Notice>}

        {status.startup && (
          <div className="startup">
            <div>{status.startup.step}</div>
            {status.startup.detail && <div className="muted small">{status.startup.detail}</div>}
            {status.startup.total > 0 && <Meter value={status.startup.done} max={status.startup.total} />}
          </div>
        )}

        {status.mount && (
          <dl className="facts">
            <dt>Mounted at</dt>
            <dd className="mono">{status.mount.mountpoint}</dd>
            <dt>Sources</dt>
            <dd>
              {status.mount.packages.length} packages, {status.mount.patches.length} patches
            </dd>
            {status.features && (
              <>
                <dt>Imagery</dt>
                <dd>{status.features.provider}</dd>
                <dt>Encoder</dt>
                <dd>
                  {status.features.encoder}
                  {status.features.encoderWarning && <span className="warn-text"> — {status.features.encoderWarning}</span>}
                </dd>
                <dt>Night lighting</dt>
                <dd>{status.features.nightLighting}</dd>
              </>
            )}
            {status.live && (
              <>
                <dt>Prefetch</dt>
                <dd>{status.live.prefetchState}</dd>
                <dt>X-Plane</dt>
                <dd>
                  {status.live.telemetryConnected ? <Badge tone="green">Connected</Badge> : <Badge tone="grey">Waiting for X-Plane</Badge>}
                  {status.live.position && (
                    <span className="mono small">
                      {" "}
                      {status.live.position[0].toFixed(3)}, {status.live.position[1].toFixed(3)}
                    </span>
                  )}
                </dd>
              </>
            )}
          </dl>
        )}

        {status.prewarm && (
          <div className="startup">
            <div>
              Pre-warming {status.prewarm.icao} {status.prewarm.airport && `(${status.prewarm.airport})`}:{" "}
              {count(status.prewarm.completed)} / {count(status.prewarm.total)} tiles
              {status.prewarm.complete && (status.prewarm.cancelled ? " — cancelled" : " — done")}
            </div>
            <Meter value={status.prewarm.completed} max={status.prewarm.total} />
            {!status.prewarm.complete && (
              <button className="small-button" onClick={() => api.cancelPrewarm()}>
                Cancel pre-warm
              </button>
            )}
          </div>
        )}
      </Card>

      {status.telemetry && status.features && <TelemetryPanel t={status.telemetry} features={status.features} />}
      {!status.telemetry && status.lastSession && (
        <Card title="Last session">
          <SessionSummary t={status.lastSession} />
        </Card>
      )}
    </div>
  );
}

function TelemetryPanel({ t, features }: { t: Telemetry; features: NonNullable<ServiceStatus["features"]> }) {
  return (
    <>
      <div className="grid-4">
        <Stat label="Tiles served" value={count(t.fuseTilesServed)} sub={`${t.fuseRequestsActive} active, ${t.fuseRequestsWaiting} waiting`} />
        <Stat label="Download" value={rate(t.bytesPerSecond)} sub={`peak ${rate(t.peakBytesPerSecond)}`} />
        <Stat label="Jobs" value={count(t.jobsCompleted)} sub={`${t.jobsActive} running, ${t.jobsFailed} failed, ${t.jobsTimedOut} timed out`} />
        <Stat label="Uptime" value={duration(t.uptimeSecs)} sub={`${t.jobsPerSecond.toFixed(1)} jobs/s`} />
      </div>
      <Card title="Cache">
        <div className="cache-rows">
          <CacheRow label="Memory" used={t.memoryCacheBytes} max={features.memoryCacheMax} hit={t.memoryHitRate} />
          <CacheRow label="DDS disk" used={t.ddsDiskCacheBytes} max={features.ddsDiskCacheMax} hit={t.ddsDiskHitRate} />
          <CacheRow label="Chunk disk" used={t.chunkDiskCacheBytes} max={features.chunkDiskCacheMax} hit={t.chunkDiskHitRate} />
        </div>
      </Card>
      <Card title="Pipeline">
        <SessionSummary t={t} />
      </Card>
    </>
  );
}

function CacheRow({ label, used, max, hit }: { label: string; used: number; max: number; hit: number }) {
  return (
    <div className="cache-row">
      <div className="cache-label">{label}</div>
      <Meter value={used} max={max} label={label} />
      <div className="cache-figures mono small">
        {bytes(used)} / {bytes(max)} · {percent(hit)} hits
      </div>
    </div>
  );
}

function SessionSummary({ t }: { t: Telemetry }) {
  return (
    <dl className="facts columns">
      <dt>Uptime</dt>
      <dd>{duration(t.uptimeSecs)}</dd>
      <dt>Tiles served</dt>
      <dd>{count(t.fuseTilesServed)}</dd>
      <dt>Jobs</dt>
      <dd>
        {count(t.jobsSubmitted)} submitted, {count(t.jobsCompleted)} completed, {count(t.jobsCoalesced)} coalesced
      </dd>
      <dt>Chunks</dt>
      <dd>
        {count(t.chunksDownloaded)} downloaded, {count(t.chunksFailed)} failed, {count(t.chunksRetried)} retried
      </dd>
      <dt>Downloaded</dt>
      <dd>{bytes(t.bytesDownloaded)}</dd>
      <dt>Encodes</dt>
      <dd>
        {count(t.encodesCompleted)} ({t.encodesActive} active)
      </dd>
    </dl>
  );
}
