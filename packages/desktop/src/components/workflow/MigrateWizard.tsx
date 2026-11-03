import { useCallback,useState } from "react";
import { AlertTriangle,ArrowRight,CheckCircle2,FileCode,X } from "lucide-react";
import { rpc } from "../../ipc";
import type { MigratePreview } from "../../lib/workflow-types";

interface Props {
  repoId: string;
  onComplete: (path: string) => void;
  onClose: () => void;
}

type Step = "detect" | "preview" | "confirm" | "done";

export function MigrateWizard({ repoId, onComplete, onClose }: Props) {
  const [step, setStep] = useState<Step>("detect");
  const [preview, setPreview] = useState<MigratePreview | null>(null);
  const [outputPath, setOutputPath] = useState("crab.yaml");
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);

  const handleDetect = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const result = await rpc.workflowMigratePreview(repoId);
      setPreview(result);
      setStep("preview");
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setLoading(false);
    }
  }, [repoId]);

  const handleApply = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      await rpc.workflowMigrateApply(repoId, outputPath);
      setStep("done");
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setLoading(false);
    }
  }, [repoId, outputPath]);

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/50">
      <div className="w-[600px] max-h-[80vh] flex flex-col rounded-lg border border-gx-border bg-gx-surface shadow-xl">
        {/* Header */}
        <div className="flex items-center justify-between border-b border-gx-border px-4 py-3">
          <h2 className="text-sm font-medium text-gx-text">Migrate from DVC</h2>
          <button
            type="button"
            className="rounded p-1 text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text"
            onClick={onClose}
          >
            <X size={16} />
          </button>
        </div>

        {/* Content */}
        <div className="flex-1 overflow-y-auto p-4">
          {step === "detect" && (
            <div className="flex flex-col items-center gap-4 py-8">
              <FileCode size={48} className="text-gx-muted" />
              <div className="text-center">
                <div className="mb-1 text-sm text-gx-text">
                  Convert your DVC pipeline to Crab
                </div>
                <div className="text-xs text-gx-muted">
                  This will read <code className="rounded bg-gx-bg px-1">dvc.yaml</code> and
                  generate an equivalent <code className="rounded bg-gx-bg px-1">crab.yaml</code>.
                </div>
              </div>
              <button
                type="button"
                className="btn btn-primary"
                onClick={handleDetect}
                disabled={loading}
              >
                {loading ? "Scanning..." : "Scan for DVC Pipeline"}
              </button>
            </div>
          )}

          {step === "preview" && preview && (
            <div className="space-y-4">
              <div className="flex items-center gap-2 text-sm text-gx-text">
                <CheckCircle2 size={16} className="text-gx-success" />
                Found {preview.stage_count} stage{preview.stage_count !== 1 ? "s" : ""}
              </div>

              {/* Warnings */}
              {preview.warnings.length > 0 && (
                <div className="rounded border border-gx-warn/30 bg-gx-warn/10 p-3">
                  <div className="mb-1 flex items-center gap-1 text-xs font-medium text-gx-warn">
                    <AlertTriangle size={12} /> Warnings ({preview.warnings.length})
                  </div>
                  <ul className="space-y-0.5 text-xs text-gx-warn">
                    {preview.warnings.map((w, i) => (
                      <li key={i}>• {w}</li>
                    ))}
                  </ul>
                </div>
              )}

              {/* Preview */}
              <div>
                <div className="mb-1 text-xs font-medium text-gx-muted">
                  Generated crab.yaml:
                </div>
                <pre className="max-h-60 overflow-auto rounded border border-gx-border bg-gx-bg p-3 font-mono text-xs text-gx-text">
                  {preview.crab_yaml}
                </pre>
              </div>
            </div>
          )}

          {step === "confirm" && (
            <div className="space-y-4">
              <div className="text-sm text-gx-text">
                Write the converted pipeline to:
              </div>
              <input
                className="input-field w-full font-mono text-sm"
                value={outputPath}
                onChange={(e) => setOutputPath(e.target.value)}
              />
              {outputPath === "crab.yaml" && (
                <div className="text-xs text-gx-warn">
                  ⚠ This will overwrite any existing crab.yaml
                </div>
              )}
            </div>
          )}

          {step === "done" && (
            <div className="flex flex-col items-center gap-4 py-8">
              <CheckCircle2 size={48} className="text-gx-success" />
              <div className="text-center">
                <div className="mb-1 text-sm text-gx-text">Migration complete!</div>
                <div className="text-xs text-gx-muted">
                  Your pipeline has been written to{" "}
                  <code className="rounded bg-gx-bg px-1">{outputPath}</code>
                </div>
              </div>
            </div>
          )}

          {/* Error */}
          {error && (
            <div className="mt-4 rounded border border-gx-error/30 bg-gx-error/10 p-3 text-xs text-gx-error">
              {error}
            </div>
          )}
        </div>

        {/* Footer */}
        <div className="flex items-center justify-end gap-2 border-t border-gx-border px-4 py-3">
          {step === "preview" && (
            <>
              <button type="button" className="btn text-xs" onClick={onClose}>
                Cancel
              </button>
              <button
                type="button"
                className="btn btn-primary flex items-center gap-1 text-xs"
                onClick={() => setStep("confirm")}
              >
                Continue <ArrowRight size={12} />
              </button>
            </>
          )}
          {step === "confirm" && (
            <>
              <button type="button" className="btn text-xs" onClick={() => setStep("preview")}>
                Back
              </button>
              <button
                type="button"
                className="btn btn-primary text-xs"
                onClick={handleApply}
                disabled={loading}
              >
                {loading ? "Writing..." : "Migrate"}
              </button>
            </>
          )}
          {step === "done" && (
            <button
              type="button"
              className="btn btn-primary text-xs"
              onClick={() => onComplete(outputPath)}
            >
              Open in Editor
            </button>
          )}
        </div>
      </div>
    </div>
  );
}
