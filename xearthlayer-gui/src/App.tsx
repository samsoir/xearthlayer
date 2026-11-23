import { useEffect, useState } from "react";
import { api, AppInfo } from "./api";
import { Notice, usePoll } from "./components/ui";
import { Dashboard } from "./views/Dashboard";
import { Maintenance } from "./views/Maintenance";
import { Packages } from "./views/Packages";
import { Settings } from "./views/Settings";
import { Setup } from "./views/Setup";

type Page = "dashboard" | "packages" | "setup" | "settings" | "maintenance";

const PAGES: { id: Page; label: string; icon: string }[] = [
  { id: "dashboard", label: "Dashboard", icon: "◉" },
  { id: "packages", label: "Packages", icon: "▦" },
  { id: "setup", label: "Setup", icon: "✦" },
  { id: "settings", label: "Settings", icon: "≡" },
  { id: "maintenance", label: "Maintenance", icon: "⚙" },
];

export function App() {
  const [info, setInfo] = useState<AppInfo | null>(null);
  const [page, setPage] = useState<Page>("dashboard");
  const { data: status } = usePoll(api.serviceStatus, 2000);

  const loadInfo = () => api.appInfo().then(setInfo).catch(() => setInfo(null));
  useEffect(() => {
    loadInfo().then(() => undefined);
  }, []);
  // A first start goes straight to Setup.
  useEffect(() => {
    if (info && !info.configExists) setPage("setup");
  }, [info?.configExists]);

  return (
    <div className="shell">
      <nav className="sidebar">
        <div className="brand">
          <div className="brand-mark" aria-hidden>
            ◐
          </div>
          <div>
            <div className="brand-name">XEarthLayer</div>
            <div className="brand-version">{info ? `v${info.version}` : ""}</div>
          </div>
        </div>
        {PAGES.map((p) => (
          <button key={p.id} className={page === p.id ? "nav active" : "nav"} onClick={() => setPage(p.id)}>
            <span className="nav-icon" aria-hidden>
              {p.icon}
            </span>
            {p.label}
            {p.id === "dashboard" && status?.phase === "running" && <span className="dot" title="Running" />}
          </button>
        ))}
        <div className="sidebar-footer">
          {info && (
            <>
              <div title={info.configPath}>Config: {info.configExists ? "config.ini" : "defaults"}</div>
              <div title={info.logFile}>Log: xearthlayer-gui.log</div>
            </>
          )}
        </div>
      </nav>
      <main className="content">
        {info && !info.configExists && page !== "setup" && (
          <Notice kind="info">
            XEarthLayer is not set up yet.{" "}
            <button className="link" onClick={() => setPage("setup")}>
              Run Setup
            </button>
          </Notice>
        )}
        {page === "dashboard" && <Dashboard />}
        {page === "packages" && <Packages />}
        {page === "setup" && <Setup onSaved={loadInfo} />}
        {page === "settings" && <Settings />}
        {page === "maintenance" && <Maintenance />}
      </main>
    </div>
  );
}
