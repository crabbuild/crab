import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Textarea } from "@/components/ui/textarea";
import { CheckCircle2, Loader2, Plus, Trash2 } from "lucide-react";
import { useCallback, useEffect, useId, useRef, useState } from "react";
import type { StorageConnectionInfo, StorageConnectionInput, StorageConnectionStatus, StorageCredentials } from "../../electron/storage-connection-types";
import { rpc, type StorageVerifyResult } from "../ipc";
import { useActiveRepo, type OpenRepo } from "../state";

interface Props {
  repo?: OpenRepo;
  onSelect?: (id: string) => void;
  selectedId?: string;
}

const EMPTY = { name: "", provider: "s3" as StorageCredentials["provider"], bucket: "", region: "us-east-1", endpoint: "", accessKey: "", secretKey: "", sessionToken: "", serviceAccount: "", account: "", azureMethod: "key", azureSecret: "" };

export function StorageConfigSection({ repo, onSelect, selectedId }: Props = {}) {
  const currentRepo = useActiveRepo();
  const activeRepo = repo ?? currentRepo;
  const repoId = onSelect ? undefined : activeRepo?.repo_id;
  const [status, setStatus] = useState<StorageConnectionStatus | null>(null);
  const [form, setForm] = useState(EMPTY);
  const [adding, setAdding] = useState(false);
  const [editingId, setEditingId] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [result, setResult] = useState<StorageVerifyResult | null>(null);
  const [message, setMessage] = useState<string | null>(null);
  const generation = useRef(0);
  const formId = useId();

  const refresh = useCallback(async () => {
    const request = ++generation.current;
    try {
      const next = await rpc.storageConnections(repoId);
      if (request === generation.current) setStatus(next);
    } catch (error) {
      if (request === generation.current) setError(error instanceof Error ? error.message : String(error));
    }
  }, [repoId]);

  useEffect(() => {
    setStatus(null); setError(null); setResult(null); setMessage(null);
    void refresh();
    return () => { generation.current++; };
  }, [refresh]);

  function update<K extends keyof typeof EMPTY>(
    key: K,
    value: (typeof EMPTY)[K],
  ) {
    setForm(previous => ({ ...previous, [key]: value }));
    setResult(null); setMessage(null); setError(null);
  }

  function input(): StorageConnectionInput {
    let credentials: StorageCredentials;
    if (form.provider === "s3") credentials = { provider: "s3", access_key_id: form.accessKey, secret_access_key: form.secretKey, session_token: form.sessionToken || undefined };
    else if (form.provider === "gcs") credentials = { provider: "gcs", service_account_key: form.serviceAccount };
    else credentials = { provider: "azure", account: form.account, ...(form.azureMethod === "key" ? { access_key: form.azureSecret } : { sas_token: form.azureSecret }) };
    return { ...(editingId ? { id: editingId } : {}), name: form.name, provider: form.provider, bucket: form.bucket, region: form.provider === "s3" ? form.region : "", endpoint: form.endpoint, credentials };
  }

  function edit(connection: StorageConnectionInfo) {
    setForm({ ...EMPTY, name: connection.name, provider: connection.provider, bucket: connection.bucket, region: connection.region, endpoint: connection.endpoint });
    setEditingId(connection.id); setAdding(true); setError(null); setResult(null); setMessage(null);
  }

  async function act(action: string, operation: () => Promise<void>) {
    setBusy(action); setError(null); setMessage(null);
    try { await operation(); }
    catch (error) { setError(error instanceof Error ? error.message : String(error)); }
    finally { setBusy(null); }
  }

  async function select(id: string) {
    if (onSelect) { onSelect(id); return; }
    if (!repoId) return;
    await act("select", async () => {
      try { await rpc.storageConnectionBind(repoId, id || null); }
      finally { await refresh(); }
      setMessage(id ? "Connection selected for this repository." : "Default provider credentials selected.");
    });
  }

  const selected = selectedId ?? status?.selected_id ?? "";
  const inputClass = "w-full rounded border border-border bg-background px-3 py-2 text-sm text-foreground outline-none focus:border-primary";
  const field = (
    label: string,
    key: keyof typeof EMPTY,
    secret = false,
    hint?: string,
  ) => (
    <div className="min-w-0 space-y-1 text-xs text-muted-foreground">
      <label className="block" htmlFor={`${formId}-${key}`}>
        {label}
      </label>
      <Input
        id={`${formId}-${key}`} aria-describedby={hint ? `${formId}-${key}-hint` : undefined} className={inputClass} type={secret ? "password" : "text"} value={String(form[key])} onChange={event => update(key, event.target.value)} autoComplete={secret ? "new-password" : "off"} spellCheck={false} disabled={Boolean(busy)}
      />
      {hint && (
        <span id={`${formId}-${key}-hint`} className="block text-[11px]">
          {hint}
        </span>
      )}
    </div>
  );

  return (
    <section className="space-y-4 p-4" aria-label="Storage connections">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h3 className="text-sm font-semibold">Storage connections</h3>
          <p className="text-xs text-muted-foreground">
            Save credentials for S3-compatible storage, Google Cloud Storage,
            and Azure Blob.
          </p>
        </div>
        <Button variant="outline" type="button" className="text-xs" onClick={() => { setAdding(value => !value); setEditingId(null); setForm(EMPTY); setError(null); setResult(null); }} disabled={Boolean(busy)}>
          <Plus size={14} />
          {adding ? "Cancel" : "Add connection"}
        </Button>
      </div>
      {error && (
        <div role="alert" className="rounded border border-destructive/40 bg-destructive/10 p-3 text-xs text-destructive break-words">
          {error}
        </div>
      )}
      {message && (
        <p role="status" className="text-xs text-success">
          {message}
        </p>
      )}
      {status && !status.secure_storage && (
        <p role="alert" className="text-xs text-warning">
          Unlock or configure your operating system keyring to save credentials
          securely.
        </p>
      )}
      {!status && !error && (
        <p className="text-xs text-muted-foreground">Loading connections…</p>
      )}
      {status && (
        <div className="space-y-2">
          {(onSelect || repoId) && (
            <Button variant="outline" type="button" className="text-xs" onClick={() => void select("")} disabled={Boolean(busy)}>
              {selected === "" && <CheckCircle2 size={13} />}Use default
              credentials
            </Button>
          )}
          {status.connections.length === 0 && (
            <p className="text-xs text-muted-foreground">
              No saved connections. Add one to authenticate without configuring
              credentials in a terminal.
            </p>
          )}
          {status.connections.map((connection) => (
            <div key={connection.id} className="flex min-w-0 flex-wrap items-center gap-3 rounded border border-border p-3">
              <div className="min-w-[8rem] flex-1">
                <div className="flex items-center gap-2 text-sm">
                  {selected === connection.id && (
                    <CheckCircle2 size={14} className="shrink-0 text-success" />
                  )}
                  <span className="truncate">{connection.name}</span>
                </div>
                <div className="break-all text-xs text-muted-foreground">
                  {connection.provider.toUpperCase()} · {connection.bucket}
                  {connection.endpoint ? ` · ${connection.endpoint}` : ""}
                </div>
              </div>
              {(onSelect || repoId) && (
                <Button variant="outline" type="button" className="shrink-0 text-xs" onClick={() => void select(connection.id)} disabled={Boolean(busy)}>
                  Use connection
                </Button>
              )}
              <Button variant="outline" type="button" className="shrink-0 text-xs" aria-label={`Edit ${connection.name}`} disabled={Boolean(busy)} onClick={() => edit(connection)}>
                Edit
              </Button>
              <Button variant="outline" type="button" className="shrink-0 text-xs" aria-label={`Test ${connection.name}`} disabled={Boolean(busy)} onClick={() => void act("test", async () => {
                const tested = await rpc.storageConnectionTest(connection.id, repoId);
                if (!tested.ok) throw new Error(tested.error);
                setMessage(`${connection.name}: bucket read/list access verified (${tested.latency_ms} ms).`);
              })}>
                Test
              </Button>
              <Button variant="outline" type="button" className="shrink-0" aria-label={`Forget ${connection.name}`} disabled={Boolean(busy)} onClick={() => void act("remove", async () => { await rpc.storageConnectionRemove(connection.id); if (selectedId === connection.id) onSelect?.(""); await refresh(); })}>
                <Trash2 size={13} />
              </Button>
            </div>
          ))}
        </div>
      )}
      {adding && (
        <form className="space-y-3 rounded border border-border bg-card p-4" onSubmit={event => { event.preventDefault(); void act("save", async () => {
          try {
            const saved = await rpc.storageConnectionSave(input());
            setForm(EMPTY); setAdding(false); setEditingId(null); setResult(null);
            onSelect?.(saved.id);
            setMessage("Connection saved with operating system encryption.");
          } finally { await refresh(); }
        }); }}>
          {editingId && (
            <p className="text-xs text-muted-foreground">
              Re-enter credentials to update this connection. Open repositories
              reconnect when desktop services are available.
            </p>
          )}
          {field("Connection name", "name")}
          <label className="block space-y-1 text-xs text-muted-foreground">
            <span>Storage provider</span>
            <Select
              value={":" + String(form.provider)}
              disabled={Boolean(busy)}
              onValueChange={(selectedValue) => {
                const value = selectedValue.slice(1);
                setForm({
                  ...EMPTY,
                  name: form.name,
                  provider: value as StorageCredentials["provider"],
                });
                setResult(null);
                setError(null);
              }}
            >
              <SelectTrigger className={inputClass}>
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectGroup>
                  <SelectItem value={":" + String("s3")}>
                    Amazon S3 / S3-compatible
                  </SelectItem>
                  <SelectItem value={":" + String("gcs")}>
                    Google Cloud Storage
                  </SelectItem>
                  <SelectItem value={":" + String("azure")}>
                    Azure Blob Storage
                  </SelectItem>
                </SelectGroup>
              </SelectContent>
            </Select>
          </label>
          {field(form.provider === "azure" ? "Container" : "Bucket", "bucket")}
          {form.provider === "s3" && (
            <>
              {field("Signing region", "region")}
              {field("Endpoint URL (optional)", "endpoint", false, "For R2, MinIO, RustFS, or another S3-compatible service. Leave empty for AWS.")}
              {field("Access key ID", "accessKey")}
              {field("Secret access key", "secretKey", true)}
              {field("Session token (optional)", "sessionToken", true)}
            </>
          )}
          {form.provider === "gcs" && (
            <label className="block space-y-1 text-xs text-muted-foreground">
              <span>Service-account JSON</span>
              <Textarea aria-label="Service-account JSON" className={`${inputClass} min-h-32 font-mono`} value={form.serviceAccount} onChange={event => update("serviceAccount", event.target.value)} spellCheck={false} autoComplete="off" disabled={Boolean(busy)} />
              <span className="block text-[11px]">
                The provider refreshes authentication tokens automatically. The
                JSON key is encrypted on this device.
              </span>
            </label>
          )}
          {form.provider === "azure" && (
            <>
              {field("Storage account", "account")}
              <label className="block space-y-1 text-xs text-muted-foreground">
                <span>Authentication method</span>
                <Select
                  value={":" + String(form.azureMethod)}
                  disabled={Boolean(busy)}
                  onValueChange={(selectedValue) => {
                    const value = selectedValue.slice(1);
                    update("azureMethod", value);
                    update("azureSecret", "");
                  }}
                >
                  <SelectTrigger className={inputClass}>
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    <SelectGroup>
                      <SelectItem value={":" + String("key")}>
                        Account key
                      </SelectItem>
                      <SelectItem value={":" + String("sas")}>
                        SAS token
                      </SelectItem>
                    </SelectGroup>
                  </SelectContent>
                </Select>
              </label>
              {field(form.azureMethod === "key" ? "Account key" : "SAS token", "azureSecret", true)}
              {field("Endpoint URL (optional)", "endpoint")}
            </>
          )}
          {result && (
            <p role={result.ok ? "status" : "alert"} className={`text-xs break-words ${result.ok ? "text-success" : "text-destructive"}`}>
              {result.ok ? `Bucket read/list access verified (${result.latency_ms} ms).` : result.error}
            </p>
          )}
          <div className="flex flex-wrap gap-2">
            <Button variant="outline" type="button" className="text-xs" disabled={Boolean(busy)} onClick={() => void act("test", async () => { setResult(await rpc.storageConnectionTest(input(), repoId)); })}>
              {busy === "test" && (
                <Loader2 size={13} className="animate-spin" />
              )}
              Test connection
            </Button>
            <Button variant="default" type="submit" className="text-xs" disabled={Boolean(busy) || !status?.secure_storage}>
              {busy === "save" && (
                <Loader2 size={13} className="animate-spin" />
              )}
              {editingId ? "Save changes" : "Save connection"}
            </Button>
          </div>
        </form>
      )}
    </section>
  );
}
