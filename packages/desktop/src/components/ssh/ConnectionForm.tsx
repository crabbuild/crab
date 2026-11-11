import { Input } from "@/components/ui/input";
import { Button } from "@/components/ui/button";
import { cloneElement, isValidElement, useEffect, useId, useRef, useState } from "react";
import clsx from "clsx";
import {
  Check,
  ChevronDown,
  ChevronUp,
  Loader2,
  Plus,
  Trash2,
  X,
  Zap,
} from "lucide-react";
import type { SshProfile } from "./ssh-profile-types";

export interface SshProfileFormData {
  id?: string;
  label: string;
  host: string;
  port: number;
  username: string;
  authMethod: SshProfile["authMethod"];
  password?: string;
  remotePath: string;
  jumpHost: string;
  forwardAgent: boolean;
  compression: boolean;
  keepAliveInterval: number;
  autoReconnect: boolean;
  autoDeployAgent: boolean;
  colour: string;
  tags: string[];
  connectionTimeoutMs?: number;
  operationTimeoutMs?: number;
  portForwards: Array<{
    id: string;
    direction: "local" | "remote";
    localPort: number;
    remoteHost: string;
    remotePort: number;
    label: string;
    autoStart: boolean;
    status: "active" | "stopped" | "error";
  }>;
}

interface ConnectionFormProps {
  profile?: SshProfile | null;
  onSave: (data: SshProfileFormData) => void;
  onCancel: () => void;
}

type TestResult = { success: true } | { success: false; error: string };

const COLOURS = ["gray", "blue", "green", "purple", "orange", "red", "pink", "cyan"];

/**
 * Add/edit SSH connection dialog. Modal overlay with form fields for
 * all connection parameters.
 *
 * Features:
 * - Basic fields: label, host, port, username, auth method
 * - Advanced section: remote path, jump host, colour, tags, options
 * - Port forwards section with add/remove
 * - "Test Connection" button with inline result
 * - Accessible: focus trap, escape to close, aria-labelledby
 */
export function ConnectionForm({ profile, onSave, onCancel }: ConnectionFormProps) {
  const titleId = useId();
  const dialogRef = useRef<HTMLDivElement>(null);
  const [showAdvanced, setShowAdvanced] = useState(false);
  const [testing, setTesting] = useState(false);
  const [testResult, setTestResult] = useState<TestResult | null>(null);

  // Form state
  const [label, setLabel] = useState(profile?.label ?? "");
  const [host, setHost] = useState(profile?.host ?? "");
  const [port, setPort] = useState(profile?.port ?? 22);
  const [username, setUsername] = useState(profile?.username ?? "");
  const [authType, setAuthType] = useState<string>(profile?.authMethod.type ?? "agent");
  const [keyPath, setKeyPath] = useState(
    profile?.authMethod.type === "key" || profile?.authMethod.type === "key_passphrase"
      ? profile.authMethod.keyPath
      : "~/.ssh/id_ed25519",
  );
  const [remotePath, setRemotePath] = useState(profile?.remotePath ?? "");
  const [password, setPassword] = useState("");
  const [jumpHost, setJumpHost] = useState(profile?.jumpHost ?? "");
  const [colour, setColour] = useState(profile?.colour ?? "gray");
  const [tagsInput, setTagsInput] = useState(profile?.tags.join(", ") ?? "");
  const [forwardAgent, setForwardAgent] = useState(profile?.forwardAgent ?? false);
  const [compression, setCompression] = useState(profile?.compression ?? false);
  const [autoReconnect, setAutoReconnect] = useState(profile?.autoReconnect ?? true);
  const [autoDeployAgent, setAutoDeployAgent] = useState(profile?.autoDeployAgent ?? true);
  const [keepAliveInterval, setKeepAliveInterval] = useState(profile?.keepAliveInterval ?? 15);
  const [connectionTimeoutMs, setConnectionTimeoutMs] = useState<number | "">(
    profile?.connectionTimeoutMs ?? "",
  );
  const [operationTimeoutMs, setOperationTimeoutMs] = useState<number | "">(
    profile?.operationTimeoutMs ?? "",
  );
  const [portForwards, setPortForwards] = useState<SshProfileFormData["portForwards"]>(
    (profile?.portForwards ?? []).map((f) => ({ ...f, label: f.label ?? "" })),
  );

  // Focus trap: focus the dialog on mount
  useEffect(() => {
    const firstInput = dialogRef.current?.querySelector<HTMLInputElement>("input");
    firstInput?.focus();
  }, []);

  // Escape to close
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") onCancel();
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [onCancel]);

  function buildFormData(): SshProfileFormData {
    let authMethod: SshProfile["authMethod"];
    switch (authType) {
      case "key":
        authMethod = { type: "key", keyPath };
        break;
      case "key_passphrase":
        authMethod = { type: "key_passphrase", keyPath };
        break;
      case "password":
        authMethod = { type: "password" };
        break;
      default:
        authMethod = { type: "agent" };
    }

    return {
      id: profile?.id,
      label: label.trim(),
      host: host.trim(),
      port,
      username: username.trim(),
      authMethod,
      password: authType === "password" ? password : undefined,
      remotePath: remotePath.trim(),
      jumpHost: jumpHost.trim(),
      forwardAgent,
      compression,
      keepAliveInterval,
      autoReconnect,
      autoDeployAgent,
      colour,
      tags: tagsInput
        .split(",")
        .map((t) => t.trim())
        .filter(Boolean),
      connectionTimeoutMs: connectionTimeoutMs === "" ? undefined : connectionTimeoutMs,
      operationTimeoutMs: operationTimeoutMs === "" ? undefined : operationTimeoutMs,
      portForwards,
    };
  }

  function handleSubmit(e: React.FormEvent) {
    e.preventDefault();
    onSave(buildFormData());
  }

  async function handleTestConnection() {
    setTesting(true);
    setTestResult(null);
    try {
      const data = buildFormData();
      const result = await window.crab.ssh.testConnection(data);
      if (result.ok) {
        setTestResult({ success: true });
      } else {
        setTestResult({
          success: false,
          error: result.error ?? "Connection failed",
        });
      }
    } catch (err) {
      setTestResult({
        success: false,
        error: err instanceof Error ? err.message : "Connection test failed",
      });
    } finally {
      setTesting(false);
    }
  }

  function addPortForward() {
    setPortForwards((prev) => [
      ...prev,
      {
        id: crypto.randomUUID(),
        direction: "local" as const,
        localPort: 8888,
        remoteHost: "127.0.0.1",
        remotePort: 8888,
        label: "",
        autoStart: true,
        status: "stopped" as const,
      },
    ]);
  }

  function removePortForward(id: string) {
    setPortForwards((prev) => prev.filter((f) => f.id !== id));
  }

  function updatePortForward(id: string, field: string, value: any) {
    setPortForwards((prev) =>
      prev.map((f) => (f.id === id ? { ...f, [field]: value } : f)),
    );
  }

  const isValid = label.trim() && host.trim() && username.trim() && port > 0 && port <= 65535;

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/50 backdrop-blur-sm"
      onClick={(e) => { if (e.target === e.currentTarget) onCancel(); }}
      role="presentation"
    >
      <div
        ref={dialogRef}
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        className="relative max-h-[90vh] w-full max-w-lg overflow-auto rounded-xl border border-border bg-background p-6 shadow-2xl"
      >
        {/* Close button */}
        <button
          type="button"
          className="absolute right-4 top-4 text-muted-foreground hover:text-foreground"
          onClick={onCancel}
          aria-label="Close dialog"
        >
          <X size={16} />
        </button>

        <h2 id={titleId} className="mb-4 text-base font-semibold text-foreground">
          {profile ? "Edit SSH Connection" : "Add SSH Connection"}
        </h2>

        <form onSubmit={handleSubmit} className="space-y-4">
          {/* Basic fields */}
          <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
            <FormField label="Label" required>
              <Input
                type="text"

                placeholder="GPU Training Server"
                value={label}
                onChange={(e) => setLabel(e.target.value)}
                required
                aria-required="true"
              />
            </FormField>
            <FormField label="Host" required>
              <Input
                type="text"

                placeholder="gpu-01.internal.example.com"
                value={host}
                onChange={(e) => setHost(e.target.value)}
                required
                aria-required="true"
              />
            </FormField>
            <FormField label="Port">
              <Input
                type="number"

                min={1}
                max={65535}
                value={port}
                onChange={(e) => setPort(Number(e.target.value))}
              />
            </FormField>
            <FormField label="Username" required>
              <Input
                type="text"

                placeholder="haipingf"
                value={username}
                onChange={(e) => setUsername(e.target.value)}
                required
                aria-required="true"
              />
            </FormField>
          </div>

          {/* Auth method */}
          <FormField label="Authentication">
            <div className="space-y-2">
              <label className="flex items-center gap-2 text-xs text-foreground cursor-pointer">
                <input
                  type="radio"
                  name="auth"
                  value="agent"
                  checked={authType === "agent"}
                  onChange={() => setAuthType("agent")}
                  className="accent-primary"
                />
                SSH Agent (recommended)
              </label>
              <label className="flex items-center gap-2 text-xs text-foreground cursor-pointer">
                <input
                  type="radio"
                  name="auth"
                  value="key"
                  checked={authType === "key"}
                  onChange={() => setAuthType("key")}
                  className="accent-primary"
                />
                Key File
              </label>
              <label className="flex items-center gap-2 text-xs text-foreground cursor-pointer">
                <input
                  type="radio"
                  name="auth"
                  value="key_passphrase"
                  checked={authType === "key_passphrase"}
                  onChange={() => setAuthType("key_passphrase")}
                  className="accent-primary"
                />
                Key File (with passphrase)
              </label>
              <label className="flex items-center gap-2 text-xs text-foreground cursor-pointer">
                <input
                  type="radio"
                  name="auth"
                  value="password"
                  checked={authType === "password"}
                  onChange={() => setAuthType("password")}
                  className="accent-primary"
                />
                Password
              </label>
            </div>
            {(authType === "key" || authType === "key_passphrase") && (
              <Input
                type="text"
                className="mt-2"
                placeholder="~/.ssh/id_ed25519"
                value={keyPath}
                onChange={(e) => setKeyPath(e.target.value)}
                aria-label="Key file path"
              />
            )}
            {authType === "password" && (
              <Input
                type="password"
                className="mt-2"
                placeholder="Enter password"
                value={password}
                onChange={(e) => setPassword(e.target.value)}
                aria-label="SSH password"
              />
            )}
          </FormField>

          {/* Advanced section */}
          <button
            type="button"
            className="flex w-full items-center gap-1 text-xs font-medium text-muted-foreground hover:text-foreground"
            onClick={() => setShowAdvanced(!showAdvanced)}
            aria-expanded={showAdvanced}
          >
            {showAdvanced ? <ChevronUp size={12} /> : <ChevronDown size={12} />}
            Advanced
          </button>

          {showAdvanced && (
            <div className="space-y-3 rounded border border-border bg-card p-3">
              <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
                <FormField label="Remote path">
                  <Input
                    type="text"

                    placeholder="/home/user/repos"
                    value={remotePath}
                    onChange={(e) => setRemotePath(e.target.value)}
                  />
                </FormField>
                <FormField label="Jump host">
                  <Input
                    type="text"

                    placeholder="bastion.example.com"
                    value={jumpHost}
                    onChange={(e) => setJumpHost(e.target.value)}
                  />
                </FormField>
              </div>

              {/* Colour picker */}
              <FormField label="Colour">
                <div className="flex items-center gap-1.5">
                  {COLOURS.map((c) => (
                    <button
                      key={c}
                      type="button"
                      className={clsx(
                        "h-5 w-5 rounded-full border-2 transition",
                        colour === c ? "border-foreground scale-110" : "border-transparent",
                      )}
                      style={{ backgroundColor: c }}
                      onClick={() => setColour(c)}
                      aria-label={`Select ${c} colour`}
                      aria-pressed={colour === c}
                    />
                  ))}
                </div>
              </FormField>

              {/* Tags */}
              <FormField label="Tags" hint="Comma-separated">
                <Input
                  type="text"

                  placeholder="gpu, training"
                  value={tagsInput}
                  onChange={(e) => setTagsInput(e.target.value)}
                />
              </FormField>

              {/* Toggles */}
              <div className="grid grid-cols-1 gap-2 sm:grid-cols-2">
                <ToggleField label="Forward SSH agent" checked={forwardAgent} onChange={setForwardAgent} />
                <ToggleField label="Enable compression" checked={compression} onChange={setCompression} />
                <ToggleField label="Auto-reconnect" checked={autoReconnect} onChange={setAutoReconnect} />
                <ToggleField label="Auto-deploy agent" checked={autoDeployAgent} onChange={setAutoDeployAgent} />
              </div>

              {/* Keep alive */}
              <FormField label="Keep-alive interval (seconds)">
                <Input
                  type="number"
                  className="w-24"
                  min={5}
                  max={300}
                  value={keepAliveInterval}
                  onChange={(e) => setKeepAliveInterval(Number(e.target.value))}
                />
              </FormField>

              {/* Timeouts */}
              <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
                <FormField label="Connection timeout (ms)" hint="Initial handshake timeout">
                  <Input
                    type="number"
                    className="w-28"
                    min={1000}
                    max={120000}
                    placeholder="10000"
                    value={connectionTimeoutMs}
                    onChange={(e) =>
                      setConnectionTimeoutMs(e.target.value === "" ? "" : Number(e.target.value))
                    }
                  />
                </FormField>
                <FormField label="Operation timeout (ms)" hint="Leave blank to let large-file reads finish. Cancellation waits for cleanup.">
                  <Input
                    type="number"
                    className="w-28"
                    min={1000}
                    max={300000}
                    placeholder="Automatic"
                    value={operationTimeoutMs}
                    onChange={(e) =>
                      setOperationTimeoutMs(e.target.value === "" ? "" : Number(e.target.value))
                    }
                  />
                </FormField>
              </div>

              {/* Port forwards */}
              <div>
                <div className="mb-2 flex items-center justify-between">
                  <span className="text-xs font-medium text-foreground">Port Forwards</span>
                  <Button variant="outline"
                    type="button"
                    className="flex items-center gap-1 text-[10px]"
                    onClick={addPortForward}
                  >
                    <Plus size={10} /> Add Forward
                  </Button>
                </div>
                {portForwards.length > 0 && (
                  <div className="space-y-2">
                    {portForwards.map((fwd) => (
                      <div key={fwd.id} className="flex items-center gap-2 rounded border border-border bg-background p-2">
                        <Input
                          type="number"
                          className="w-16 text-xs"
                          placeholder="8888"
                          value={fwd.localPort}
                          onChange={(e) => updatePortForward(fwd.id, "localPort", Number(e.target.value))}
                          aria-label="Local port"
                        />
                        <span className="text-[10px] text-muted-foreground">→</span>
                        <Input
                          type="number"
                          className="w-16 text-xs"
                          placeholder="8888"
                          value={fwd.remotePort}
                          onChange={(e) => updatePortForward(fwd.id, "remotePort", Number(e.target.value))}
                          aria-label="Remote port"
                        />
                        <Input
                          type="text"
                          className="flex-1 text-xs"
                          placeholder="Label (e.g. Jupyter)"
                          value={fwd.label}
                          onChange={(e) => updatePortForward(fwd.id, "label", e.target.value)}
                          aria-label="Forward label"
                        />
                        <label className="flex items-center gap-1 text-[10px] text-muted-foreground whitespace-nowrap cursor-pointer">
                          <input
                            type="checkbox"
                            checked={fwd.autoStart}
                            onChange={(e) => updatePortForward(fwd.id, "autoStart", e.target.checked)}
                            className="accent-primary"
                          />
                          Auto
                        </label>
                        <button
                          type="button"
                          className="text-destructive hover:text-destructive"
                          onClick={() => removePortForward(fwd.id)}
                          aria-label="Remove forward"
                        >
                          <Trash2 size={12} />
                        </button>
                      </div>
                    ))}
                  </div>
                )}
              </div>
            </div>
          )}

          {/* Test connection result */}
          {testResult && (
            <div
              className={clsx(
                "rounded border p-2 text-xs",
                testResult.success
                  ? "border-success/40 bg-success/10 text-success text-success"
                  : "border-destructive/40 bg-destructive/10 text-destructive text-destructive",
              )}
              role="alert"
            >
              {testResult.success ? (
                <span className="flex items-center gap-1">
                  <Check size={12} /> Connection successful
                </span>
              ) : (
                <div>
                  <span className="font-medium">{testResult.error}</span>
                </div>
              )}
            </div>
          )}

          {/* Footer buttons */}
          <div className="flex items-center gap-2 pt-2">
            <Button variant="outline"
              type="button"
              className="flex items-center gap-1.5 text-xs"
              onClick={handleTestConnection}
              disabled={testing || !isValid}
              aria-label="Test SSH connection"
            >
              {testing ? (
                <Loader2 size={12} className="animate-spin" />
              ) : (
                <Zap size={12} />
              )}
              Test Connection
            </Button>
            <div className="flex-1" />
            <Button variant="outline"
              type="button"
              className="text-xs"
              onClick={onCancel}
            >
              Cancel
            </Button>
            <Button variant="default"
              type="submit"
              className="text-xs"
              disabled={!isValid}
            >
              {profile ? "Save" : "Save & Connect"}
            </Button>
          </div>
        </form>
      </div>
    </div>
  );
}

// ── Helper components ────────────────────────────────────────────────

function FormField({
  label,
  hint,
  required,
  children,
}: {
  label: string;
  hint?: string;
  required?: boolean;
  children: React.ReactNode;
}) {
  const fieldId = useId();
  const control = isValidElement<{ id?: string }>(children) && typeof children.type === "string" && ["input", "select", "textarea"].includes(children.type) ? children : null;
  const controlId = control?.props.id ?? fieldId;
  return (
    <div role={control ? undefined : "group"} aria-labelledby={control ? undefined : `${fieldId}-label`}>
      <label id={`${fieldId}-label`} htmlFor={control ? controlId : undefined} className="mb-1 block text-xs font-medium text-foreground">
        {label}
        {required && <span className="ml-0.5 text-destructive">*</span>}
      </label>
      {control ? cloneElement(control, { id: controlId }) : children}
      {hint && <p className="mt-0.5 text-[10px] text-muted-foreground">{hint}</p>}
    </div>
  );
}

function ToggleField({
  label,
  checked,
  onChange,
}: {
  label: string;
  checked: boolean;
  onChange: (v: boolean) => void;
}) {
  return (
    <label className="flex items-center gap-2 text-xs text-foreground cursor-pointer">
      <input
        type="checkbox"
        checked={checked}
        onChange={(e) => onChange(e.target.checked)}
        className="accent-primary"
      />
      {label}
    </label>
  );
}
