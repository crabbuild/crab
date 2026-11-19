import {
  shortId,
  actionReason,
  actionReasons,
} from "../../../../workflow-results/model";
import { useState } from "react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Dialog,
  DialogContent,
  DialogTitle,
  DialogDescription,
  DialogFooter,
} from "@/components/ui/dialog";
import type {
  ActionPreview,
  SealedResult,
  ReportView,
  ComparisonSpec,
  EvidenceView,
} from "../../lib/results-contracts";

export function ActionPreviewDialog({
  name,
  preview,
  onClose,
  onConfirm,
}: {
  name: string;
  preview: ActionPreview;
  onClose: () => void;
  onConfirm: () => void;
}) {
  const resume = name === "resume";
  return (
    <Dialog
      open
      onOpenChange={(open) => {
        if (!open) onClose();
      }}
    >
      <DialogContent className="max-h-[85vh] overflow-y-auto">
        <DialogTitle>
          {resume ? "Resume checkpoint" : "Apply captured workspace"}
        </DialogTitle>
        <DialogDescription>
          {resume
            ? "Continue this checkpoint in a new experiment using current HEAD and current workflow configuration."
            : "Replace these workspace paths with captured files. Local edits at these paths will be overwritten; listed deletions will be removed."}
        </DialogDescription>
        <p className="break-all text-xs">
          {resume ? "Current HEAD" : "Source commit"} ·{" "}
          <span className="font-mono">{preview.base_commit}</span>
        </p>
        {preview.checkpoint && (
          <p className="break-all text-xs">
            Checkpoint · <span className="font-mono">{preview.checkpoint}</span>
          </p>
        )}
        <div className="max-h-64 overflow-auto text-xs">
          <h4 className="font-medium">
            {resume ? "Checkpoint outputs" : "Files to write"} ·{" "}
            {preview.applied.length}
          </h4>
          {preview.applied.map((path) => (
            <p key={path} className="break-all py-0.5 font-mono">
              {path}
            </p>
          ))}
          {preview.deleted.length > 0 && (
            <>
              <h4 className="mt-3 font-medium text-destructive">
                Files to delete · {preview.deleted.length}
              </h4>
              {preview.deleted.map((path) => (
                <p key={path} className="break-all py-0.5 font-mono">
                  {path}
                </p>
              ))}
            </>
          )}
        </div>
        <DialogFooter>
          <Button variant="outline" onClick={onClose}>
            Cancel
          </Button>
          <Button
            variant={resume ? "default" : "destructive"}
            onClick={onConfirm}
          >
            {resume ? "Resume" : "Apply workspace"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

export function RunDetails({
  run,
  report,
  spec,
  evidence,
  canAct,
  busy,
  onAction,
  onEvidence,
  onChange,
}: {
  run: SealedResult;
  report: ReportView | null;
  spec: ComparisonSpec;
  evidence: EvidenceView | null;
  canAct: boolean;
  busy: boolean;
  onAction: (name: string) => void;
  onEvidence: (path: string) => void;
  onChange: (change: Partial<ComparisonSpec>) => void;
}) {
  return (
    <div className="mt-4 border-t border-border pt-4">
      <div className="flex flex-wrap items-center gap-2">
        <h3 className="mr-auto text-sm font-medium">
          Run evidence · {shortId(run.snapshot.experiment_id)}
        </h3>
        {["apply", "reproduce", "resume", "promote", "branch", "publish"].map(
          (name) => (
            <Button
              key={name}
              size="sm"
              variant="outline"
              disabled={
                !canAct ||
                busy ||
                Boolean(report) ||
                Boolean(actionReason(run, name))
              }
              title={
                actionReason(run, name) ||
                (name === "branch"
                  ? "Create Git branch"
                  : name === "resume"
                    ? "Continue checkpoint using current workflow configuration"
                    : undefined)
              }
              onClick={() => void onAction(name)}
            >
              {name === "branch"
                ? "Create branch"
                : name === "promote"
                  ? "Promote artifact"
                  : name[0].toUpperCase() + name.slice(1)}
            </Button>
          ),
        )}
      </div>
      <ul className="mt-2 space-y-1 text-xs text-muted-foreground">
        {actionReasons(run, Boolean(report), canAct).map((reason) => (
          <li key={reason}>{reason}</li>
        ))}
      </ul>
      <details className="mt-2 text-xs text-muted-foreground">
        <summary className="cursor-pointer">Captured run identities</summary>
        <p className="mt-2 break-all font-mono text-xs text-muted-foreground">
          Run {run.snapshot.experiment_id} · Source commit{" "}
          {run.snapshot.base_commit} · Snapshot {run.digest}
        </p>
        {run.snapshot.reproduction?.source_result && (
          <p className="mt-2 break-all font-mono text-xs text-muted-foreground">
            Reproduced from {run.snapshot.reproduction.source_result.id} ·
            Source snapshot {run.snapshot.reproduction.source_result.digest}
          </p>
        )}
      </details>
      <DatasetControls
        run={run}
        spec={report?.report.comparison.spec ?? spec}
        onChange={onChange}
        immutable={Boolean(report)}
      />
      <div className="mt-3 grid gap-4 lg:grid-cols-2">
        <div>
          <h4 className="text-xs font-medium">Stages and checkpoints</h4>
          {run.snapshot.stages.map((stage) => (
            <div
              key={stage.name}
              className="border-b border-border py-2 text-xs"
            >
              <strong>{stage.name}</strong> · {stage.state} ·{" "}
              {stage.duration_ms === null
                ? "Duration unavailable"
                : String(stage.duration_ms) + " ms"}{" "}
              · {stage.attempts} attempts
              <details>
                <summary className="cursor-pointer text-muted-foreground">
                  Dataset/output witnesses
                </summary>
                {stage.hash && (
                  <p className="break-all font-mono">
                    Stage {stage.hash} · {stage.source}
                  </p>
                )}
                {stage.inputs.map((input) => (
                  <p
                    key={JSON.stringify([input.path, input.hash])}
                    className="break-all py-1 font-mono"
                  >
                    Input {input.path ?? "(external source)"} · {input.hash}
                  </p>
                ))}
                {stage.outputs.map((output) => (
                  <p
                    key={JSON.stringify([output.path, output.hash])}
                    className="break-all py-1 font-mono"
                  >
                    Output {output.path} · {output.hash}
                  </p>
                ))}
              </details>
            </div>
          ))}
          <h4 className="mt-4 text-xs font-medium">
            Retained artifacts and logs
          </h4>
          {run.snapshot.attachments
            .filter(
              (a) =>
                !report ||
                a.kind !== "log" ||
                report.report.comparison.spec.include_logs,
            )
            .map((a) => (
              <Button
                key={`${a.path}:${a.stage}`}
                variant="ghost"
                className="h-auto w-full justify-start whitespace-normal break-all px-0 py-1 text-left text-xs"
                onClick={() => void onEvidence(a.path)}
              >
                {a.kind} · {a.path} · {a.state}
              </Button>
            ))}
          {run.snapshot.capture_issues.map((issue) => (
            <p key={issue} className="text-xs text-warning">
              {issue}
            </p>
          ))}
        </div>
        <div>
          {evidence ? (
            <>
              <h4 className="break-all text-xs font-medium">
                {evidence.attachment.path}
              </h4>
              {evidence.reason && (
                <p className="py-2 text-xs text-muted-foreground">
                  {evidence.reason}
                </p>
              )}
              {evidence.data_url && (
                <img
                  src={evidence.data_url}
                  alt={evidence.attachment.path}
                  className="mt-2 max-h-96 max-w-full object-contain"
                />
              )}
              {evidence.text !== null && (
                <pre className="mt-2 max-h-96 overflow-auto whitespace-pre-wrap break-all rounded bg-muted p-3 font-mono text-xs">
                  {evidence.text}
                </pre>
              )}
            </>
          ) : (
            <p className="text-xs text-muted-foreground">
              Choose a retained artifact or log to inspect its captured bytes.
            </p>
          )}
        </div>
      </div>
    </div>
  );
}
function DatasetControls({
  run,
  spec,
  onChange,
  immutable,
}: {
  run: SealedResult;
  spec: ComparisonSpec;
  onChange: (change: Partial<ComparisonSpec>) => void;
  immutable: boolean;
}) {
  const id = run.snapshot.experiment_id;
  const current = spec.datasets[id] ?? { hash: "", split: "", evaluator: "" };
  // The parent keys drafts by snapshot/report, so re-querying a comparison
  // cannot erase fields the user is still editing.
  const [draft, setDraft] = useState(current);
  return (
    <details className="mt-3 text-xs">
      <summary className="cursor-pointer text-muted-foreground">
        Dataset and split identity · {current.hash ? current.split : "Unknown"}
      </summary>
      <p className="my-2 text-muted-foreground">
        {immutable
          ? "Dataset, split and evaluator identity retained in this report."
          : "Use a retained input hash and name the split and evaluator. Improvement labels require matching identities."}
      </p>
      <div className="grid grid-cols-2 gap-2">
        <label className="col-span-2 min-w-0 text-muted-foreground">
          <span className="mb-1 block">Dataset hash</span>
          <Input
            aria-label="Dataset hash"
            readOnly={immutable}
            list={`dataset-witnesses-${id}`}
            className="h-8 w-full font-mono text-xs text-foreground"
            value={draft.hash}
            onChange={(e) => setDraft({ ...draft, hash: e.target.value })}
            placeholder={immutable ? "Unknown" : "BLAKE3 input hash"}
          />
        </label>
        <datalist id={`dataset-witnesses-${id}`}>
          {[
            ...new Map(
              [
                ...run.snapshot.stages
                  .filter((stage) => stage.state === "Committed")
                  .flatMap((stage) => stage.inputs),
                ...(run.snapshot.reproduction?.files
                  .filter((file) => file.state === "present" && file.hash)
                  .map((file) => ({ path: file.path, hash: file.hash! })) ??
                  []),
              ].map((input) => [input.hash, input]),
            ).values(),
          ].map((input) => (
            <option key={input.hash} value={input.hash}>
              {input.path ?? "External input"}
            </option>
          ))}
        </datalist>
        <label className="min-w-0 text-muted-foreground">
          <span className="mb-1 block">Split</span>
          <Input
            aria-label="Dataset split"
            readOnly={immutable}
            className="h-8 w-full text-xs text-foreground"
            value={draft.split}
            onChange={(e) => setDraft({ ...draft, split: e.target.value })}
            placeholder={immutable ? "Unknown" : "validation"}
          />
        </label>
        <label className="min-w-0 text-muted-foreground">
          <span className="mb-1 block">Evaluator</span>
          <Input
            aria-label="Evaluator identity"
            readOnly={immutable}
            className="h-8 w-full text-xs text-foreground"
            value={draft.evaluator}
            onChange={(e) => setDraft({ ...draft, evaluator: e.target.value })}
            placeholder={immutable ? "Unknown" : "Evaluator version"}
          />
        </label>
        {!immutable && (
          <Button
            className="col-span-2 justify-self-start"
            size="sm"
            variant="outline"
            disabled={
              !/^[0-9a-f]{64}$/.test(draft.hash) ||
              !draft.split ||
              !draft.evaluator
            }
            onClick={() =>
              onChange({ datasets: { ...spec.datasets, [id]: draft } })
            }
          >
            Set identity
          </Button>
        )}
      </div>
    </details>
  );
}
