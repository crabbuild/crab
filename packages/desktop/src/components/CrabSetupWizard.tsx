import { useCallback, useState } from "react";
import clsx from "clsx";
import {
  CheckCircle2,
  Cloud,
  Eye,
  EyeOff,
  HardDrive,
  Loader2,
  Package,
  Server,
  XCircle,
} from "lucide-react";
import { rpc, StorageVerifyResult } from "../ipc";

type Provider = "s3" | "gcs" | "azure";

type WizardStep = "provider" | "bucket" | "credentials" | "verify";

interface Props {
  repoId: string;
  /** Called when initialization + configuration is complete. */
  onComplete: () => void;
}

/**
 * Inline setup wizard shown in the Large Files view when the repo
 * is not yet initialized for crab. Guides the user through:
 *
 * 1. Choosing a storage provider (S3, GCS, Azure)
 * 2. Entering bucket/container + prefix
 * 3. (Optional) credentials
 * 4. Verifying connectivity + running `crab init`
 */
export function CrabSetupWizard({ repoId, onComplete }: Props) {
  const [step, setStep] = useState<WizardStep>("provider");
  const [provider, setProvider] = useState<Provider>("s3");
  const [bucket, setBucket] = useState("");
  const [prefix, setPrefix] = useState("");
  const [region, setRegion] = useState("us-east-1");
  const [accessKeyId, setAccessKeyId] = useState("");
  const [secretAccessKey, setSecretAccessKey] = useState("");
  const [gcpServiceAccountJson, setGcpServiceAccountJson] = useState("");
  const [azureConnectionString, setAzureConnectionString] = useState("");
  const [showSecret, setShowSecret] = useState(false);
  const [saving, setSaving] = useState(false);
  const [verifying, setVerifying] = useState(false);
  const [verifyResult, setVerifyResult] = useState<StorageVerifyResult | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [initDone, setInitDone] = useState(false);

  function buildUrl(): string {
    const p = prefix.trim();
    switch (provider) {
      case "s3":
        return `crab://${bucket.trim()}${p ? `/${p}` : ""}`;
      case "gcs":
        return `gs://${bucket.trim()}${p ? `/${p}` : ""}`;
      case "azure":
        return `az://${bucket.trim()}${p ? `/${p}` : ""}`;
    }
  }

  const handleInitAndConfigure = useCallback(async () => {
    setSaving(true);
    setError(null);

    try {
      // storageConfigSet creates .crab/config.toml, sets up the git
      // remote, stores credentials in the OS keychain, and registers
      // the filter driver via `crab init` — all in one call.
      const credentials: Record<string, string> = {};
      if (provider === "s3") {
        if (accessKeyId) credentials.access_key_id = accessKeyId;
        if (secretAccessKey) credentials.secret_access_key = secretAccessKey;
      } else if (provider === "gcs") {
        if (gcpServiceAccountJson) credentials.service_account_json = gcpServiceAccountJson;
      } else if (provider === "azure") {
        if (azureConnectionString) credentials.connection_string = azureConnectionString;
      }

      await rpc.storageConfigSet(repoId, {
        provider,
        bucket: bucket.trim(),
        prefix: prefix.trim() || undefined,
        region: provider === "s3" ? region : undefined,
        credentials: Object.keys(credentials).length > 0 ? credentials : undefined,
      });

      setInitDone(true);
      setStep("verify");
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  }, [repoId, provider, bucket, prefix, region, accessKeyId, secretAccessKey, gcpServiceAccountJson, azureConnectionString]);

  const handleVerify = useCallback(async () => {
    setVerifying(true);
    setVerifyResult(null);
    setError(null);
    try {
      const result = await rpc.storageConfigVerify(repoId);
      setVerifyResult(result);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setVerifying(false);
    }
  }, [repoId]);

  const steps: { key: WizardStep; label: string }[] = [
    { key: "provider", label: "Provider" },
    { key: "bucket", label: "Bucket" },
    { key: "credentials", label: "Credentials" },
    { key: "verify", label: "Connect" },
  ];

  const currentStepIndex = steps.findIndex((s) => s.key === step);

  return (
    <div className="flex h-full flex-col items-center justify-start overflow-auto p-6">
      <div className="w-full max-w-lg space-y-6">
        {/* Header */}
        <div className="text-center">
          <div className="mx-auto mb-3 flex h-12 w-12 items-center justify-center rounded-full bg-gx-accent/10">
            <Package size={24} className="text-gx-accent" />
          </div>
          <h2 className="text-lg font-semibold text-gx-text">Setup Large File Storage</h2>
          <p className="mt-1 text-sm text-gx-muted">
            Connect an S3-compatible bucket to store large files with
            content-defined chunking and deduplication.
          </p>
        </div>

        {/* Step indicator */}
        <div className="flex items-center justify-center gap-1">
          {steps.map((s, i) => (
            <div key={s.key} className="flex items-center">
              <div
                className={clsx(
                  "flex h-6 w-6 items-center justify-center rounded-full text-[10px] font-bold transition",
                  i < currentStepIndex
                    ? "bg-gx-success text-white"
                    : i === currentStepIndex
                      ? "bg-gx-accent text-white"
                      : "bg-gx-border text-gx-muted",
                )}
              >
                {i < currentStepIndex ? <CheckCircle2 size={12} /> : i + 1}
              </div>
              {i < steps.length - 1 && (
                <div
                  className={clsx(
                    "mx-1 h-0.5 w-8 rounded",
                    i < currentStepIndex ? "bg-gx-success" : "bg-gx-border",
                  )}
                />
              )}
            </div>
          ))}
        </div>

        {/* Step: Provider */}
        {step === "provider" && (
          <div className="space-y-4">
            <label className="block text-xs font-medium text-gx-text">
              Choose your storage provider
            </label>
            <div className="grid grid-cols-3 gap-3">
              {([
                { key: "s3" as Provider, label: "AWS S3", desc: "S3-compatible", icon: Cloud },
                { key: "gcs" as Provider, label: "Google Cloud", desc: "GCS buckets", icon: Server },
                { key: "azure" as Provider, label: "Azure Blob", desc: "Containers", icon: HardDrive },
              ]).map((p) => (
                <button
                  key={p.key}
                  type="button"
                  className={clsx(
                    "flex flex-col items-center gap-2 rounded-lg border p-4 text-center transition",
                    provider === p.key
                      ? "border-gx-accent bg-gx-accent/5 text-gx-accent"
                      : "border-gx-border text-gx-muted hover:border-gx-accent/50 hover:text-gx-text",
                  )}
                  onClick={() => setProvider(p.key)}
                >
                  <p.icon size={20} />
                  <span className="text-xs font-medium">{p.label}</span>
                  <span className="text-[10px] text-gx-muted">{p.desc}</span>
                </button>
              ))}
            </div>
            <p className="text-[11px] text-gx-muted">
              Any S3-compatible service works (MinIO, Cloudflare R2, Backblaze B2, etc.).
              Select "AWS S3" for these.
            </p>
            <div className="flex justify-end">
              <button
                type="button"
                className="btn btn-primary text-xs"
                onClick={() => setStep("bucket")}
              >
                Next →
              </button>
            </div>
          </div>
        )}

        {/* Step: Bucket */}
        {step === "bucket" && (
          <div className="space-y-4">
            <div>
              <label className="mb-1 block text-xs font-medium text-gx-text">
                {provider === "azure" ? "Container Name" : "Bucket Name"}
              </label>
              <input
                type="text"
                className="w-full rounded border border-gx-border bg-gx-bg-sub px-3 py-2 text-sm focus-visible:outline-none focus:border-gx-accent"
                value={bucket}
                onChange={(e) => setBucket(e.target.value)}
                placeholder={provider === "azure" ? "my-container" : "my-crab-storage"}
                autoFocus
              />
            </div>
            <div>
              <label className="mb-1 block text-xs font-medium text-gx-text">
                Prefix <span className="text-gx-muted">(optional — namespaces repos within the bucket)</span>
              </label>
              <input
                type="text"
                className="w-full rounded border border-gx-border bg-gx-bg-sub px-3 py-2 text-sm focus-visible:outline-none focus:border-gx-accent"
                value={prefix}
                onChange={(e) => setPrefix(e.target.value)}
                placeholder="team/ml-models"
              />
            </div>
            {provider === "s3" && (
              <div>
                <label className="mb-1 block text-xs font-medium text-gx-text">Region</label>
                <input
                  type="text"
                  className="w-full rounded border border-gx-border bg-gx-bg-sub px-3 py-2 text-sm focus-visible:outline-none focus:border-gx-accent"
                  value={region}
                  onChange={(e) => setRegion(e.target.value)}
                  placeholder="us-east-1"
                />
              </div>
            )}

            {/* URL preview */}
            {bucket.trim() && (
              <div className="rounded border border-gx-border bg-gx-bg-sub p-2">
                <div className="text-[10px] uppercase tracking-wider text-gx-muted">
                  Remote URL
                </div>
                <div className="mt-1 font-mono text-xs text-gx-text">{buildUrl()}</div>
              </div>
            )}

            <div className="flex justify-between">
              <button
                type="button"
                className="btn text-xs"
                onClick={() => setStep("provider")}
              >
                ← Back
              </button>
              <button
                type="button"
                className="btn btn-primary text-xs"
                onClick={() => setStep("credentials")}
                disabled={!bucket.trim()}
              >
                Next →
              </button>
            </div>
          </div>
        )}

        {/* Step: Credentials */}
        {step === "credentials" && (
          <div className="space-y-4">
            <div className="rounded border border-gx-border p-4">
              <div className="mb-3 flex items-center justify-between">
                <span className="text-xs font-medium text-gx-text">
                  Credentials <span className="text-gx-muted">(optional)</span>
                </span>
                <button
                  type="button"
                  className="flex items-center gap-1 text-xs text-gx-muted hover:text-gx-text"
                  onClick={() => setShowSecret(!showSecret)}
                >
                  {showSecret ? <EyeOff size={10} /> : <Eye size={10} />}
                  {showSecret ? "Hide" : "Show"}
                </button>
              </div>

              {provider === "s3" && (
                <div className="space-y-2">
                  <input
                    type={showSecret ? "text" : "password"}
                    className="w-full rounded border border-gx-border bg-gx-bg-sub px-3 py-2 text-sm focus-visible:outline-none focus:border-gx-accent"
                    value={accessKeyId}
                    onChange={(e) => setAccessKeyId(e.target.value)}
                    placeholder="AWS Access Key ID"
                    autoComplete="off"
                  />
                  <input
                    type={showSecret ? "text" : "password"}
                    className="w-full rounded border border-gx-border bg-gx-bg-sub px-3 py-2 text-sm focus-visible:outline-none focus:border-gx-accent"
                    value={secretAccessKey}
                    onChange={(e) => setSecretAccessKey(e.target.value)}
                    placeholder="AWS Secret Access Key"
                    autoComplete="off"
                  />
                </div>
              )}

              {provider === "gcs" && (
                <textarea
                  className="h-24 w-full rounded border border-gx-border bg-gx-bg-sub p-2 font-mono text-xs focus-visible:outline-none focus:border-gx-accent"
                  value={gcpServiceAccountJson}
                  onChange={(e) => setGcpServiceAccountJson(e.target.value)}
                  placeholder="Paste service account JSON here..."
                />
              )}

              {provider === "azure" && (
                <input
                  type={showSecret ? "text" : "password"}
                  className="w-full rounded border border-gx-border bg-gx-bg-sub px-3 py-2 text-sm focus-visible:outline-none focus:border-gx-accent"
                  value={azureConnectionString}
                  onChange={(e) => setAzureConnectionString(e.target.value)}
                  placeholder="DefaultEndpointsProtocol=https;AccountName=..."
                  autoComplete="off"
                />
              )}

              <p className="mt-3 text-[10px] text-gx-muted">
                Leave empty to use environment variables or IAM roles.
                Credentials are stored in your OS keychain, never on disk.
              </p>
            </div>

            {error && (
              <div className="rounded border border-gx-error/40 bg-gx-error/10 p-2 text-xs text-gx-error">
                {error}
              </div>
            )}

            <div className="flex justify-between">
              <button
                type="button"
                className="btn text-xs"
                onClick={() => { setStep("bucket"); setError(null); }}
              >
                ← Back
              </button>
              <button
                type="button"
                className="btn btn-primary text-xs"
                onClick={handleInitAndConfigure}
                disabled={saving}
              >
                {saving ? (
                  <>
                    <Loader2 size={12} className="animate-spin" /> Initializing...
                  </>
                ) : (
                  "Initialize & Connect →"
                )}
              </button>
            </div>
          </div>
        )}

        {/* Step: Verify */}
        {step === "verify" && (
          <div className="space-y-4">
            {initDone && (
              <div className="rounded border border-gx-success/30 bg-gx-success/5 p-3">
                <div className="flex items-center gap-2 text-sm font-medium text-gx-success">
                  <CheckCircle2 size={14} />
                  Repository initialized
                </div>
                <p className="mt-1 text-xs text-gx-muted">
                  Filter driver registered. Storage configured at{" "}
                  <code className="rounded bg-gx-bg-sub px-1 text-gx-text">{buildUrl()}</code>
                </p>
              </div>
            )}

            <div className="flex flex-col items-center gap-3">
              {!verifyResult && !verifying && (
                <button
                  type="button"
                  className="btn btn-primary text-xs"
                  onClick={handleVerify}
                >
                  Verify Connection
                </button>
              )}

              {verifying && (
                <div className="flex items-center gap-2 text-xs text-gx-muted">
                  <Loader2 size={12} className="animate-spin" /> Testing connectivity...
                </div>
              )}

              {verifyResult && (
                <div
                  className={clsx(
                    "flex w-full items-center gap-2 rounded border p-3 text-xs",
                    verifyResult.ok
                      ? "border-gx-success/30 bg-gx-success/5 text-gx-success"
                      : "border-gx-error/40 bg-gx-error/10 text-gx-error",
                  )}
                >
                  {verifyResult.ok ? <CheckCircle2 size={14} /> : <XCircle size={14} />}
                  {verifyResult.ok
                    ? `Connected successfully (${verifyResult.latency_ms}ms)`
                    : verifyResult.error ?? "Connection failed"}
                </div>
              )}
            </div>

            {error && (
              <div className="rounded border border-gx-error/40 bg-gx-error/10 p-2 text-xs text-gx-error">
                {error}
              </div>
            )}

            <div className="flex justify-between">
              <button
                type="button"
                className="btn text-xs"
                onClick={() => { setStep("credentials"); setVerifyResult(null); setError(null); }}
              >
                ← Back
              </button>
              <button
                type="button"
                className="btn btn-primary text-xs"
                onClick={onComplete}
              >
                {verifyResult?.ok ? "Start Tracking Files →" : "Skip Verification →"}
              </button>
            </div>
          </div>
        )}
      </div>
    </div>
  );
}
