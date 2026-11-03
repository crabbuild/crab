import { useCallback, useEffect, useState } from "react";
import clsx from "clsx";
import {
  CheckCircle2,
  Eye,
  EyeOff,
  Loader2,
  RefreshCw,
  Save,
  Trash2,
  XCircle,
} from "lucide-react";
import { rpc, StorageVerifyResult } from "../ipc";
import { useActiveRepo } from "../state";

type Provider = "s3" | "gcs" | "azure";

interface FormState {
  provider: Provider;
  bucket: string;
  prefix: string;
  region: string;
  accessKeyId: string;
  secretAccessKey: string;
  gcpServiceAccountJson: string;
  azureConnectionString: string;
}

const EMPTY_FORM: FormState = {
  provider: "s3",
  bucket: "",
  prefix: "",
  region: "us-east-1",
  accessKeyId: "",
  secretAccessKey: "",
  gcpServiceAccountJson: "",
  azureConnectionString: "",
};

/**
 * Remote storage configuration section for the Settings view.
 * Allows users to configure S3/GCS/Azure credentials and bucket
 * settings so they can push directly to their remote from the app.
 */
export function StorageConfigSection() {
  const activeRepo = useActiveRepo();
  const [form, setForm] = useState<FormState>(EMPTY_FORM);
  const [loading, setLoading] = useState(false);
  const [saving, setSaving] = useState(false);
  const [verifying, setVerifying] = useState(false);
  const [verifyResult, setVerifyResult] = useState<StorageVerifyResult | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [success, setSuccess] = useState<string | null>(null);
  const [showSecret, setShowSecret] = useState(false);

  // Load existing config when repo changes.
  const loadConfig = useCallback(async () => {
    if (!activeRepo) return;
    setLoading(true);
    setError(null);
    try {
      const config = await rpc.storageConfigGet(activeRepo.repo_id);
      if (config.configured && config.provider) {
        setForm((prev) => ({
          ...prev,
          provider: (config.provider as Provider) ?? "s3",
          bucket: config.bucket ?? "",
          prefix: config.prefix ?? "",
          region: config.region ?? "us-east-1",
        }));
      }
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }, [activeRepo]);

  useEffect(() => {
    loadConfig();
  }, [loadConfig]);

  async function handleSave() {
    if (!activeRepo) return;
    setSaving(true);
    setError(null);
    setSuccess(null);
    try {
      const credentials: Record<string, string> = {};
      if (form.provider === "s3") {
        if (form.accessKeyId) credentials.access_key_id = form.accessKeyId;
        if (form.secretAccessKey) credentials.secret_access_key = form.secretAccessKey;
      } else if (form.provider === "gcs") {
        if (form.gcpServiceAccountJson) credentials.service_account_json = form.gcpServiceAccountJson;
      } else if (form.provider === "azure") {
        if (form.azureConnectionString) credentials.connection_string = form.azureConnectionString;
      }

      await rpc.storageConfigSet(activeRepo.repo_id, {
        provider: form.provider,
        bucket: form.bucket,
        prefix: form.prefix || undefined,
        region: form.region || undefined,
        credentials: Object.keys(credentials).length > 0 ? credentials : undefined,
      });
      setSuccess("Configuration saved");
      setTimeout(() => setSuccess(null), 3000);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  }

  async function handleVerify() {
    if (!activeRepo) return;
    setVerifying(true);
    setVerifyResult(null);
    setError(null);
    try {
      const result = await rpc.storageConfigVerify(activeRepo.repo_id);
      setVerifyResult(result);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setVerifying(false);
    }
  }

  function handleClear() {
    setForm(EMPTY_FORM);
    setVerifyResult(null);
    setError(null);
    setSuccess(null);
  }

  function update<K extends keyof FormState>(key: K, value: FormState[K]) {
    setForm((prev) => ({ ...prev, [key]: value }));
  }

  if (!activeRepo) {
    return (
      <div className="p-4 text-sm text-gx-muted">
        Open a repository to configure remote storage.
      </div>
    );
  }

  if (loading) {
    return (
      <div className="flex items-center gap-2 p-4 text-sm text-gx-muted">
        <Loader2 size={14} className="animate-spin" /> Loading configuration...
      </div>
    );
  }

  // Compute the remote URL preview.
  const remoteUrl = form.bucket
    ? form.provider === "s3"
      ? `crab://${form.bucket}${form.prefix ? `/${form.prefix}` : ""}`
      : form.provider === "gcs"
        ? `gs://${form.bucket}${form.prefix ? `/${form.prefix}` : ""}`
        : `az://${form.bucket}${form.prefix ? `/${form.prefix}` : ""}`
    : null;

  return (
    <div className="space-y-4">
      {/* Provider selector */}
      <div>
        <label className="mb-1 block text-xs font-medium text-gx-text">Provider</label>
        <div className="flex gap-2">
          {(["s3", "gcs", "azure"] as Provider[]).map((p) => (
            <button
              key={p}
              type="button"
              className={clsx(
                "rounded border px-3 py-1.5 text-xs font-medium transition",
                form.provider === p
                  ? "border-gx-accent bg-gx-accent/10 text-gx-accent"
                  : "border-gx-border text-gx-muted hover:border-gx-accent/50",
              )}
              onClick={() => update("provider", p)}
            >
              {p === "s3" ? "AWS S3" : p === "gcs" ? "Google Cloud" : "Azure Blob"}
            </button>
          ))}
        </div>
      </div>

      {/* Bucket / Container */}
      <div>
        <label className="mb-1 block text-xs font-medium text-gx-text">
          {form.provider === "azure" ? "Container" : "Bucket"}
        </label>
        <input
          type="text"
          className="w-full rounded border border-gx-border bg-gx-bg-sub px-3 py-1.5 text-sm focus-visible:outline-none focus:border-gx-accent"
          value={form.bucket}
          onChange={(e) => update("bucket", e.target.value)}
          placeholder={form.provider === "azure" ? "my-container" : "my-bucket"}
        />
      </div>

      {/* Prefix */}
      <div>
        <label className="mb-1 block text-xs font-medium text-gx-text">
          Prefix <span className="text-gx-muted">(optional)</span>
        </label>
        <input
          type="text"
          className="w-full rounded border border-gx-border bg-gx-bg-sub px-3 py-1.5 text-sm focus-visible:outline-none focus:border-gx-accent"
          value={form.prefix}
          onChange={(e) => update("prefix", e.target.value)}
          placeholder="team/ml-models"
        />
      </div>

      {/* Region (S3 only) */}
      {form.provider === "s3" && (
        <div>
          <label className="mb-1 block text-xs font-medium text-gx-text">Region</label>
          <input
            type="text"
            className="w-full rounded border border-gx-border bg-gx-bg-sub px-3 py-1.5 text-sm focus-visible:outline-none focus:border-gx-accent"
            value={form.region}
            onChange={(e) => update("region", e.target.value)}
            placeholder="us-east-1"
          />
        </div>
      )}

      {/* Credentials */}
      <div className="rounded border border-gx-border p-3">
        <div className="mb-2 flex items-center justify-between">
          <span className="text-xs font-medium text-gx-text">Credentials</span>
          <button
            type="button"
            className="flex items-center gap-1 text-xs text-gx-muted hover:text-gx-text"
            onClick={() => setShowSecret(!showSecret)}
          >
            {showSecret ? <EyeOff size={10} /> : <Eye size={10} />}
            {showSecret ? "Hide" : "Show"}
          </button>
        </div>

        {form.provider === "s3" && (
          <div className="space-y-2">
            <input
              type={showSecret ? "text" : "password"}
              className="w-full rounded border border-gx-border bg-gx-bg-sub px-3 py-1.5 text-sm focus-visible:outline-none focus:border-gx-accent"
              value={form.accessKeyId}
              onChange={(e) => update("accessKeyId", e.target.value)}
              placeholder="AWS Access Key ID"
              autoComplete="off"
            />
            <input
              type={showSecret ? "text" : "password"}
              className="w-full rounded border border-gx-border bg-gx-bg-sub px-3 py-1.5 text-sm focus-visible:outline-none focus:border-gx-accent"
              value={form.secretAccessKey}
              onChange={(e) => update("secretAccessKey", e.target.value)}
              placeholder="AWS Secret Access Key"
              autoComplete="off"
            />
          </div>
        )}

        {form.provider === "gcs" && (
          <textarea
            className="h-24 w-full rounded border border-gx-border bg-gx-bg-sub p-2 font-mono text-xs focus-visible:outline-none focus:border-gx-accent"
            value={form.gcpServiceAccountJson}
            onChange={(e) => update("gcpServiceAccountJson", e.target.value)}
            placeholder="Paste service account JSON here..."
          />
        )}

        {form.provider === "azure" && (
          <input
            type={showSecret ? "text" : "password"}
            className="w-full rounded border border-gx-border bg-gx-bg-sub px-3 py-1.5 text-sm focus-visible:outline-none focus:border-gx-accent"
            value={form.azureConnectionString}
            onChange={(e) => update("azureConnectionString", e.target.value)}
            placeholder="DefaultEndpointsProtocol=https;AccountName=..."
            autoComplete="off"
          />
        )}

        <p className="mt-2 text-[10px] text-gx-muted">
          Credentials are stored in your OS keychain and never written to disk in plaintext.
        </p>
      </div>

      {/* Remote URL preview */}
      {remoteUrl && (
        <div className="rounded border border-gx-border bg-gx-bg-sub p-2">
          <div className="text-[10px] uppercase tracking-wider text-gx-muted">Remote URL</div>
          <div className="mt-1 font-mono text-xs text-gx-text">{remoteUrl}</div>
        </div>
      )}

      {/* Actions */}
      <div className="flex items-center gap-2">
        <button
          type="button"
          className="btn btn-primary text-xs"
          onClick={handleSave}
          disabled={saving || !form.bucket}
        >
          {saving ? <Loader2 size={12} className="animate-spin" /> : <Save size={12} />}
          Save
        </button>
        <button
          type="button"
          className="btn text-xs"
          onClick={handleVerify}
          disabled={verifying || !form.bucket}
        >
          {verifying ? <Loader2 size={12} className="animate-spin" /> : <RefreshCw size={12} />}
          Verify Connection
        </button>
        <button type="button" className="btn text-xs" onClick={handleClear}>
          <Trash2 size={12} /> Clear
        </button>
      </div>

      {/* Verify result */}
      {verifyResult && (
        <div
          className={clsx(
            "flex items-center gap-2 rounded border p-2 text-xs",
            verifyResult.ok
              ? "border-gx-success/30 bg-gx-success/5 text-gx-success"
              : "border-gx-error/40 bg-gx-error/10 text-gx-error",
          )}
        >
          {verifyResult.ok ? <CheckCircle2 size={12} /> : <XCircle size={12} />}
          {verifyResult.ok
            ? `Connected (${verifyResult.latency_ms}ms)`
            : verifyResult.error ?? "Connection failed"}
        </div>
      )}

      {/* Error / Success messages */}
      {error && (
        <div className="rounded border border-gx-error/40 bg-gx-error/10 p-2 text-xs text-gx-error">
          {error}
        </div>
      )}
      {success && (
        <div className="rounded border border-gx-success/30 bg-gx-success/5 p-2 text-xs text-gx-success">
          {success}
        </div>
      )}
    </div>
  );
}
