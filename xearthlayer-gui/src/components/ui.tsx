import { ReactNode, useCallback, useEffect, useRef, useState } from "react";
import { errorText } from "../api";

/// Call `load` now and every `intervalMs`, keeping the last answer.
export function usePoll<T>(load: () => Promise<T>, intervalMs: number) {
  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState<string | null>(null);
  const loadRef = useRef(load);
  loadRef.current = load;

  const refresh = useCallback(async () => {
    try {
      setData(await loadRef.current());
      setError(null);
    } catch (e) {
      setError(errorText(e));
    }
  }, []);

  useEffect(() => {
    refresh();
    if (intervalMs <= 0) return;
    const timer = window.setInterval(refresh, intervalMs);
    return () => window.clearInterval(timer);
  }, [refresh, intervalMs]);

  return { data, error, refresh };
}

/// Run an action, tracking whether it is in flight and what it said.
export function useAction() {
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState<{ kind: "ok" | "error"; text: string } | null>(null);

  const run = useCallback(async <T,>(action: () => Promise<T>, success?: (value: T) => string | null) => {
    setBusy(true);
    setMessage(null);
    try {
      const value = await action();
      const text = success?.(value);
      if (text) setMessage({ kind: "ok", text });
      return value;
    } catch (e) {
      setMessage({ kind: "error", text: errorText(e) });
      return undefined;
    } finally {
      setBusy(false);
    }
  }, []);

  return { busy, message, setMessage, run };
}

export function Card({ title, actions, children, className }: {
  title?: ReactNode;
  actions?: ReactNode;
  children: ReactNode;
  className?: string;
}) {
  return (
    <section className={`card ${className ?? ""}`}>
      {(title || actions) && (
        <header className="card-header">
          {title && <h2>{title}</h2>}
          {actions && <div className="card-actions">{actions}</div>}
        </header>
      )}
      {children}
    </section>
  );
}

export function Stat({ label, value, sub }: { label: string; value: ReactNode; sub?: ReactNode }) {
  return (
    <div className="stat">
      <div className="stat-label">{label}</div>
      <div className="stat-value">{value}</div>
      {sub && <div className="stat-sub">{sub}</div>}
    </div>
  );
}

export function Meter({ value, max, label }: { value: number; max: number; label?: string }) {
  const fraction = max > 0 ? Math.min(1, value / max) : 0;
  return (
    <div className="meter" role="progressbar" aria-valuenow={Math.round(fraction * 100)} aria-label={label}>
      <div className="meter-fill" style={{ width: `${fraction * 100}%` }} />
    </div>
  );
}

export function Notice({ kind, children }: { kind: "ok" | "error" | "warn" | "info"; children: ReactNode }) {
  return <div className={`notice notice-${kind}`}>{children}</div>;
}

export function ActionMessage({ message }: { message: { kind: "ok" | "error"; text: string } | null }) {
  if (!message) return null;
  return <Notice kind={message.kind}>{message.text}</Notice>;
}

export function Badge({ tone, children }: { tone: "green" | "amber" | "red" | "blue" | "grey"; children: ReactNode }) {
  return <span className={`badge badge-${tone}`}>{children}</span>;
}

export function Tabs<T extends string>({ tabs, active, onChange }: {
  tabs: { id: T; label: ReactNode }[];
  active: T;
  onChange: (id: T) => void;
}) {
  return (
    <div className="tabs" role="tablist">
      {tabs.map((t) => (
        <button
          key={t.id}
          role="tab"
          aria-selected={t.id === active}
          className={t.id === active ? "tab active" : "tab"}
          onClick={() => onChange(t.id)}
        >
          {t.label}
        </button>
      ))}
    </div>
  );
}

/// A button that asks once more before doing something hard to undo.
/// Never uses `window.confirm`, which blocks the webview.
export function ConfirmButton({ label, confirmLabel, onConfirm, disabled, className }: {
  label: string;
  confirmLabel?: string;
  onConfirm: () => void;
  disabled?: boolean;
  className?: string;
}) {
  const [armed, setArmed] = useState(false);
  useEffect(() => {
    if (!armed) return;
    const timer = window.setTimeout(() => setArmed(false), 4000);
    return () => window.clearTimeout(timer);
  }, [armed]);
  return (
    <button
      className={`${className ?? ""} ${armed ? "danger" : ""}`}
      disabled={disabled}
      onClick={() => {
        if (armed) {
          setArmed(false);
          onConfirm();
        } else {
          setArmed(true);
        }
      }}
    >
      {armed ? confirmLabel ?? "Click again to confirm" : label}
    </button>
  );
}
