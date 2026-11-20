import {
  shortId,
  actionReason,
  actionReasons,
} from "../../../../workflow-results/model";
import { useState } from "react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Badge } from "@/components/ui/badge";
import {
  Empty,
  EmptyDescription,
  EmptyHeader,
  EmptyMedia,
} from "@/components/ui/empty";
import {
  ClipboardPaste,
  FileText,
  GitBranch,
  Layers3,
  Package,
  Play,
  RotateCcw,
  Upload,
} from "lucide-react";
import {
  Dialog,
  DialogContent,
  DialogTitle,
  DialogDescription,
  DialogFooter,
  DialogHeader,
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
        <DialogHeader>
          <DialogTitle>
            {resume ? "Resume checkpoint" : "Apply captured workspace"}
          </DialogTitle>
          <DialogDescription>
            {resume
              ? "Continue this checkpoint in a new experiment using current HEAD and current workflow configuration."
              : "Replace these workspace paths with captured files. Local edits at these paths will be overwritten; listed deletions will be removed."}
          </DialogDescription>
        </DialogHeader>
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
  const reasons = actionReasons(run, Boolean(report), canAct);
  return (
    <div className="mt-8 border-t border-border pt-6">
      <div className="flex flex-wrap items-center gap-x-4 gap-y-3">
        <h3 className="mr-auto inline-flex items-center gap-2 text-sm font-medium">
          <Layers3
            size={15}
            className="text-muted-foreground"
            aria-hidden="true"
          />
          Run evidence{" "}
          <Badge
            variant="secondary"
            className="font-mono text-[11px] font-normal"
          >
            {shortId(run.snapshot.experiment_id)}
          </Badge>
        </h3>
        <div className="flex flex-wrap items-center gap-2">
          {[
            { name: "apply", Icon: ClipboardPaste },
            { name: "reproduce", Icon: RotateCcw },
            { name: "resume", Icon: Play },
            { name: "promote", Icon: Package },
            { name: "branch", Icon: GitBranch },
            { name: "publish", Icon: Upload },
          ].map(({ name, Icon }) => (
            <Button
              key={name}
              size="sm"
              variant={
                name === "apply" || name === "reproduce" || name === "resume"
                  ? "outline"
                  : "ghost"
              }
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
              <Icon size={14} aria-hidden="true" />
              {name === "branch"
                ? "Create branch"
                : name === "promote"
                  ? "Promote artifact"
                  : name[0].toUpperCase() + name.slice(1)}
            </Button>
          ))}
        </div>
      </div>
      {reasons.length > 0 && (
        <details
          open={run.snapshot.origin === "experiment_metadata"}
          className="mt-3 text-xs text-muted-foreground"
        >
          <summary className="w-fit cursor-pointer">
            Action availability
          </summary>
          <ul className="mt-2 max-w-prose space-y-1.5 leading-relaxed">
            {reasons.map((reason) => (
              <li key={reason}>{reason}</li>
            ))}
          </ul>
        </details>
      )}
      <div className="mt-4 grid min-w-0 gap-x-8 gap-y-4 lg:grid-cols-2">
        <details className="min-w-0 text-xs text-muted-foreground">
          <summary className="cursor-pointer py-2">
            Captured run identities
          </summary>
          <dl className="mt-3 grid grid-cols-[auto_minmax(0,1fr)] gap-x-3 gap-y-2 text-[11px]">
            <dt>Run</dt>
            <dd className="break-all font-mono text-foreground">
              {run.snapshot.experiment_id}
            </dd>
            <dt>Source commit</dt>
            <dd className="break-all font-mono text-foreground">
              {run.snapshot.base_commit}
            </dd>
            <dt>Snapshot</dt>
            <dd className="break-all font-mono text-foreground">
              {run.digest}
            </dd>
          </dl>
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
      </div>
      <div className="mt-6 grid min-w-0 items-start gap-8 lg:grid-cols-2">
        <div className="min-w-0">
          <h4 className="mb-1 text-xs font-medium">Stages and checkpoints</h4>
          {run.snapshot.stages.map((stage) => (
            <div
              key={stage.name}
              className="border-b border-border/60 py-3 text-xs"
            >
              <div className="flex items-center justify-between gap-3">
                <strong className="truncate font-medium">{stage.name}</strong>
                <Badge
                  variant="outline"
                  className="text-[10px] font-normal text-muted-foreground"
                >
                  {stage.state}
                </Badge>
              </div>
              <p className="mt-1 text-[11px] text-muted-foreground">
                {stage.duration_ms === null
                  ? "Duration unavailable"
                  : String(stage.duration_ms) + " ms"}{" "}
                · {stage.attempts} attempts
              </p>
              <details className="mt-2">
                <summary className="cursor-pointer text-[11px] text-muted-foreground">
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
                    className="truncate py-1 font-mono"
                    title={`Input ${input.path ?? "(external source)"} · ${input.hash}`}
                  >
                    Input {input.path ?? "(external source)"} · {input.hash}
                  </p>
                ))}
                {stage.outputs.map((output) => (
                  <p
                    key={JSON.stringify([output.path, output.hash])}
                    className="truncate py-1 font-mono"
                    title={`Output ${output.path} · ${output.hash}`}
                  >
                    Output {output.path} · {output.hash}
                  </p>
                ))}
              </details>
            </div>
          ))}
          <h4 className="mb-2 mt-5 text-xs font-medium">
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
                className="h-auto min-h-9 w-full justify-start gap-2 px-2 py-2 text-left text-xs"
                aria-label={`${a.kind} · ${a.path} · ${a.state}`}
                aria-pressed={evidence?.attachment.path === a.path}
                onClick={() => void onEvidence(a.path)}
              >
                {a.kind === "log" ? (
                  <FileText size={14} className="text-muted-foreground" />
                ) : (
                  <Package size={14} className="text-muted-foreground" />
                )}
                <span
                  className="min-w-0 flex-1 truncate font-mono"
                  title={a.path}
                >
                  {a.path}
                </span>
                <span className="text-[11px] text-muted-foreground">
                  {a.kind} · {a.state}
                </span>
              </Button>
            ))}
          {run.snapshot.capture_issues.map((issue) => (
            <p key={issue} className="text-xs text-warning">
              {issue}
            </p>
          ))}
        </div>
        <div className="min-w-0 rounded-md border border-border p-4">
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
            <Empty className="min-h-40 p-6 md:p-8">
              <EmptyHeader>
                <EmptyMedia variant="icon">
                  <FileText aria-hidden="true" />
                </EmptyMedia>
                <EmptyDescription className="text-xs">
                  Select an artifact or log to preview
                </EmptyDescription>
              </EmptyHeader>
            </Empty>
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
    <details className="min-w-0 text-xs">
      <summary className="cursor-pointer py-2 text-muted-foreground">
        Dataset and split identity · {current.hash ? current.split : "Unknown"}
      </summary>
      <p className="my-3 max-w-prose text-[11px] leading-relaxed text-muted-foreground">
        {immutable
          ? "Dataset, split and evaluator identity retained in this report."
          : "Match the retained dataset hash, split and evaluator to compare improvement."}
      </p>
      <fieldset className="grid grid-cols-1 gap-3 sm:grid-cols-2">
        <legend className="sr-only">Comparison dataset identity</legend>
        <label className="min-w-0 text-muted-foreground sm:col-span-2">
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
            className="justify-self-start sm:col-span-2"
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
      </fieldset>
    </details>
  );
}
