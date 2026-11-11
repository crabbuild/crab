import { Input } from "@/components/ui/input";
/**
 * Port forwarding management panel.
 *
 * Displays active/configured port forwards with status indicators,
 * add/remove controls, and "Open in Browser" actions.
 */

import React, { useState, useCallback } from "react";
import { Globe, Plus, Trash2, ArrowRightLeft, AlertCircle } from "lucide-react";

// ── Types ────────────────────────────────────────────────────────

interface ForwardInfo {
  id: string;
  profileId: string;
  localPort: number;
  actualLocalPort: number;
  remoteHost: string;
  remotePort: number;
  label?: string;
  autoStart: boolean;
  status: "active" | "stopped" | "error";
  error?: string;
}

interface PortForwardPanelProps {
  profileId: string;
  forwards: ForwardInfo[];
  onAdd: (
    localPort: number,
    remoteHost: string,
    remotePort: number,
    label?: string,
  ) => void;
  onRemove: (forwardId: string) => void;
  onOpenBrowser: (forwardId: string) => void;
}

// ── Status indicator ─────────────────────────────────────────────

function StatusDot({ status }: { status: "active" | "stopped" | "error" }) {
  const colors: Record<string, string> = {
    active: "bg-success",
    stopped: "bg-accent",
    error: "bg-destructive",
  };

  return (
    <span
      className={`inline-block w-2 h-2 rounded-full ${colors[status]}`}
      aria-label={`Status: ${status}`}
    />
  );
}

// ── Add forward form ─────────────────────────────────────────────

function AddForwardForm({
  onAdd,
  onCancel,
}: {
  onAdd: (
    localPort: number,
    remoteHost: string,
    remotePort: number,
    label?: string,
  ) => void;
  onCancel: () => void;
}) {
  const [localPort, setLocalPort] = useState("");
  const [remoteHost, setRemoteHost] = useState("127.0.0.1");
  const [remotePort, setRemotePort] = useState("");
  const [label, setLabel] = useState("");

  const handleSubmit = useCallback(
    (e: React.FormEvent) => {
      e.preventDefault();
      const lp = parseInt(localPort, 10);
      const rp = parseInt(remotePort, 10);
      if (
        isNaN(lp) ||
        isNaN(rp) ||
        lp < 1 ||
        lp > 65535 ||
        rp < 1 ||
        rp > 65535
      ) {
        return;
      }
      onAdd(lp, remoteHost, rp, label || undefined);
    },
    [localPort, remoteHost, remotePort, label, onAdd],
  );

  return (
    <form
      onSubmit={handleSubmit}
      className="border border-border border-border rounded-md p-3 space-y-2"
    >
      <div className="grid grid-cols-2 gap-2">
        <div>
          <label
            htmlFor="pf-local-port"
            className="text-xs text-muted-foreground text-muted-foreground"
          >
            Local Port
          </label>
          <Input
            id="pf-local-port"
            type="number"
            min={1}
            max={65535}
            value={localPort}
            onChange={(e) => setLocalPort(e.target.value)}
            placeholder="8888"
            className="w-full rounded border border-border bg-background px-2 py-1.5 text-xs text-foreground outline-none focus:border-primary transition"
            required
          />
        </div>
        <div>
          <label
            htmlFor="pf-remote-port"
            className="text-xs text-muted-foreground text-muted-foreground"
          >
            Remote Port
          </label>
          <Input
            id="pf-remote-port"
            type="number"
            min={1}
            max={65535}
            value={remotePort}
            onChange={(e) => setRemotePort(e.target.value)}
            placeholder="8888"
            className="w-full rounded border border-border bg-background px-2 py-1.5 text-xs text-foreground outline-none focus:border-primary transition"
            required
          />
        </div>
      </div>
      <div>
        <label
          htmlFor="pf-remote-host"
          className="text-xs text-muted-foreground text-muted-foreground"
        >
          Remote Host
        </label>
        <Input
          id="pf-remote-host"
          type="text"
          value={remoteHost}
          onChange={(e) => setRemoteHost(e.target.value)}
          placeholder="127.0.0.1"
          className="w-full rounded border border-border bg-background px-2 py-1.5 text-xs text-foreground outline-none focus:border-primary transition"
          required
        />
      </div>
      <div>
        <label
          htmlFor="pf-label"
          className="text-xs text-muted-foreground text-muted-foreground"
        >
          Label (optional)
        </label>
        <Input
          id="pf-label"
          type="text"
          value={label}
          onChange={(e) => setLabel(e.target.value)}
          placeholder="Jupyter, TensorBoard..."
          className="w-full rounded border border-border bg-background px-2 py-1.5 text-xs text-foreground outline-none focus:border-primary transition"
        />
      </div>
      <div className="flex justify-end gap-2 pt-1">
        <button
          type="button"
          onClick={onCancel}
          className="px-3 py-1 text-xs text-muted-foreground text-muted-foreground hover:bg-card hover:bg-card rounded"
        >
          Cancel
        </button>
        <button
          type="submit"
          className="px-3 py-1 text-xs bg-info text-info-foreground rounded hover:bg-info"
        >
          Add Forward
        </button>
      </div>
    </form>
  );
}

// ── Main panel ───────────────────────────────────────────────────

export function PortForwardPanel({
  profileId,
  forwards,
  onAdd,
  onRemove,
  onOpenBrowser,
}: PortForwardPanelProps) {
  const [showForm, setShowForm] = useState(false);

  const handleAdd = useCallback(
    (
      localPort: number,
      remoteHost: string,
      remotePort: number,
      label?: string,
    ) => {
      onAdd(localPort, remoteHost, remotePort, label);
      setShowForm(false);
    },
    [onAdd],
  );

  const profileForwards = forwards.filter((f) => f.profileId === profileId);

  return (
    <div
      className="space-y-3 mx-auto w-full max-w-3xl my-4"
      role="region"
      aria-label="Port Forwards"
    >
      <div className="flex items-center justify-between">
        <h3 className="text-sm font-medium text-foreground text-muted-foreground flex items-center gap-1.5">
          <ArrowRightLeft className="w-4 h-4" />
          Port Forwards
        </h3>
        {!showForm && (
          <button
            onClick={() => setShowForm(true)}
            className="flex items-center gap-1 px-2 py-1 text-xs text-info text-info hover:bg-info/10 hover:bg-info/20 rounded"
            aria-label="Add port forward"
          >
            <Plus className="w-3 h-3" />
            Add
          </button>
        )}
      </div>

      {showForm && (
        <AddForwardForm onAdd={handleAdd} onCancel={() => setShowForm(false)} />
      )}

      {profileForwards.length === 0 && !showForm && (
        <p className="text-xs text-muted-foreground text-muted-foreground italic">
          No port forwards configured.
        </p>
      )}

      <ul className="space-y-1" aria-label="Port forward list">
        {profileForwards.map((fwd) => (
          <li
            key={fwd.id}
            className="flex items-center justify-between px-2 py-1.5 rounded hover:bg-background hover:bg-card group"
          >
            <div className="flex items-center gap-2 min-w-0">
              <StatusDot status={fwd.status} />
              <span className="text-sm truncate">
                {fwd.label && (
                  <span className="font-medium text-foreground text-muted-foreground">
                    {fwd.label}
                    {" — "}
                  </span>
                )}
                <span className="text-muted-foreground text-muted-foreground">
                  :{fwd.actualLocalPort} → {fwd.remoteHost}:{fwd.remotePort}
                </span>
                {fwd.actualLocalPort !== fwd.localPort && (
                  <span className="ml-1 text-xs text-warning text-warning">
                    (requested :{fwd.localPort})
                  </span>
                )}
              </span>
              {fwd.status === "error" && fwd.error && (
                <span
                  className="text-xs text-destructive flex items-center gap-0.5"
                  title={fwd.error}
                >
                  <AlertCircle className="w-3 h-3" />
                </span>
              )}
            </div>
            <div className="flex items-center gap-1 opacity-0 group-hover:opacity-100 transition-opacity">
              {fwd.status === "active" && (
                <button
                  onClick={() => onOpenBrowser(fwd.id)}
                  className="p-1 text-muted-foreground hover:text-info hover:text-info rounded"
                  aria-label={`Open ${fwd.label ?? "forward"} in browser`}
                  title="Open in Browser"
                >
                  <Globe className="w-3.5 h-3.5" />
                </button>
              )}
              <button
                onClick={() => onRemove(fwd.id)}
                className="p-1 text-muted-foreground hover:text-destructive hover:text-destructive rounded"
                aria-label={`Remove ${fwd.label ?? "forward"}`}
                title="Remove"
              >
                <Trash2 className="w-3.5 h-3.5" />
              </button>
            </div>
          </li>
        ))}
      </ul>
    </div>
  );
}
