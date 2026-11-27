import { useMemo, useState } from "react";
import { api, Setting, UpgradeReport } from "../api";
import { ActionMessage, Badge, Card, Notice, useAction, usePoll } from "../components/ui";

export function Settings() {
  const { data, error, refresh } = usePoll(api.configList, 0);
  const [filter, setFilter] = useState("");
  const sections = useMemo(() => {
    const f = filter.trim().toLowerCase();
    const groups = new Map<string, Setting[]>();
    for (const s of data ?? []) {
      if (f && !s.name.toLowerCase().includes(f) && !s.help.join(" ").toLowerCase().includes(f)) continue;
      groups.set(s.section, [...(groups.get(s.section) ?? []), s]);
    }
    return [...groups.entries()];
  }, [data, filter]);

  return (
    <div className="page">
      <header className="page-header">
        <div>
          <h1>Settings</h1>
          <p className="muted">Every setting in config.ini. Changes apply the next time XEarthLayer starts.</p>
        </div>
      </header>
      <Upgrade onDone={refresh} />
      <input className="search" placeholder="Search settings" value={filter} onChange={(e) => setFilter(e.target.value)} />
      {error && <Notice kind="error">{error}</Notice>}
      {sections.map(([section, settings]) => (
        <Card key={section} title={`[${section}]`}>
          <div className="settings">
            {settings.map((s) => (
              <SettingRow key={s.name} setting={s} onSaved={refresh} />
            ))}
          </div>
        </Card>
      ))}
    </div>
  );
}

function SettingRow({ setting, onSaved }: { setting: Setting; onSaved: () => void }) {
  const [value, setValue] = useState(setting.value);
  const [open, setOpen] = useState(false);
  const action = useAction();
  const dirty = setting.sensitive ? value !== "" : value !== setting.value;
  const isBool = setting.value === "true" || setting.value === "false";

  const save = (v: string) =>
    action.run(
      () => api.configSet(setting.name, v),
      (m) => {
        onSaved();
        if (setting.sensitive) setValue("");
        return m;
      },
    );

  return (
    <div className="setting">
      <div className="setting-name">
        <button className="link" onClick={() => setOpen(!open)} title="Show help">
          {setting.key}
        </button>
        {setting.sensitive && <Badge tone={setting.isSet ? "green" : "grey"}>{setting.isSet ? "set" : "not set"}</Badge>}
      </div>
      <div className="setting-value">
        {isBool ? (
          <select value={value} onChange={(e) => { setValue(e.target.value); save(e.target.value); }}>
            <option value="true">true</option>
            <option value="false">false</option>
          </select>
        ) : (
          <input
            type={setting.sensitive ? "password" : "text"}
            value={value}
            placeholder={setting.sensitive ? (setting.isSet ? "•••••• (enter to replace)" : "") : "(not set)"}
            onChange={(e) => setValue(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && dirty && save(value)}
            spellCheck={false}
          />
        )}
        {!isBool && (
          <button disabled={!dirty || action.busy} onClick={() => save(value)}>
            Save
          </button>
        )}
      </div>
      {action.message?.kind === "error" && <div className="error-text small setting-msg">{action.message.text}</div>}
      {open && setting.help.length > 0 && (
        <div className="setting-help muted small">
          {setting.help.map((p, i) => (
            <p key={i}>{p}</p>
          ))}
        </div>
      )}
    </div>
  );
}

function Upgrade({ onDone }: { onDone: () => void }) {
  const { data: report, refresh } = usePoll(() => api.configUpgrade(true), 0);
  const action = useAction();
  const init = useAction();

  if (!report) return null;
  if (!report.exists) {
    return (
      <Card title="No configuration file">
        <p>XEarthLayer is using its defaults. Run Setup, or create the file with the defaults now.</p>
        <button onClick={() => init.run(api.configInit, (p) => { onDone(); refresh(); return `Created ${p}`; })}>
          Create config.ini
        </button>
        <ActionMessage message={init.message} />
      </Card>
    );
  }
  if (!report.needsUpgrade) return null;
  return (
    <Card title="Configuration upgrade available">
      <UpgradeDetails report={report} />
      <button
        className="primary"
        disabled={action.busy}
        onClick={() =>
          action.run(
            () => api.configUpgrade(false),
            (r) => {
              onDone();
              refresh();
              return r.backup ? `Upgraded. Backup: ${r.backup}` : "Upgraded.";
            },
          )
        }
      >
        Upgrade config.ini
      </button>
      <ActionMessage message={action.message} />
    </Card>
  );
}

function UpgradeDetails({ report }: { report: UpgradeReport }) {
  const list = (title: string, items: string[]) =>
    items.length > 0 && (
      <div>
        <div className="strong small">{title}</div>
        <ul className="mono small">
          {items.map((i) => (
            <li key={i}>{i}</li>
          ))}
        </ul>
      </div>
    );
  return (
    <div className="grid-2">
      {list("Settings to add", report.missing)}
      {list("Deprecated settings to remove", report.deprecated)}
      {list("Values to rewrite", report.rewritten)}
      {list("Unknown settings (kept)", report.unknown)}
    </div>
  );
}
