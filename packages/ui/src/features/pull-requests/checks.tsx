import { Disclosure } from "@/components/ui/collapsible";
import { repoHref } from "@/lib/repository-route";
import {
  CheckCircleFillIcon,
  ClockIcon,
  CopyIcon,
  DotFillIcon,
  SkipIcon,
  XCircleFillIcon,
} from "@primer/octicons-react";
import { endpoint, useRequest, type Repository } from "@/lib/api";
import { DiscussionMarkdown } from "@/components/discussion/markdown";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { short } from "@/lib/format";
import { useState } from "react";

type CheckStatus = "queued" | "in_progress" | "completed";
type CheckConclusion =
  | "action_required"
  | "cancelled"
  | "failure"
  | "neutral"
  | "skipped"
  | "success"
  | "timed_out";

interface CheckStep {
  name: string;
  status: CheckStatus;
  conclusion: CheckConclusion | null;
  log: string | null;
}

interface CheckAnnotation {
  path: string;
  start_line: number;
  end_line: number;
  level: "notice" | "warning" | "failure";
  title: string | null;
  message: string;
}

interface CheckOutput {
  title: string;
  summary: string;
  text: string | null;
  steps: CheckStep[];
  annotations: CheckAnnotation[];
}

interface CheckRun {
  id: number;
  head_sha: string;
  name: string;
  status: CheckStatus;
  conclusion: CheckConclusion | null;
  details_url: string | null;
  output_title: string;
  author: string;
  provider?: "crab" | "github_actions";
  version: number;
  started_at: number | null;
  completed_at: number | null;
  created_at: number;
  updated_at: number;
  output?: CheckOutput;
  scan_comparison?: ScanComparison;
}

type FindingState = "introduced" | "existing" | "unclassified";

type ScanComparison = { baseline_sha: string } & (
  | {
      state: "ready";
      introduced: number;
      existing: number;
      unclassified: number;
      annotations: FindingState[];
    }
  | { state: "missing" | "incomplete" | "tools_changed" | "unreachable" }
);

const findingLabels: Record<FindingState, string> = {
  introduced: "New",
  existing: "Existing",
  unclassified: "Unclassified",
};

const comparisonMessages = {
  missing: "No matching scan has been reported for this base commit.",
  incomplete: "At least one scan did not finish successfully.",
  tools_changed: "The scans used different tools.",
  unreachable: "This base commit is unavailable.",
};

interface CheckPage {
  sha: string;
  items: CheckRun[];
  next: number | null;
}

function outcome(run: Pick<CheckRun, "status" | "conclusion">) {
  if (run.status !== "completed") return run.status;
  return run.conclusion ?? "failure";
}

function OutcomeIcon({
  status,
  conclusion,
}: Pick<CheckRun, "status" | "conclusion">) {
  const value = outcome({ status, conclusion });
  if (value === "success") return <CheckCircleFillIcon />;
  if (value === "neutral" || value === "skipped") return <SkipIcon />;
  if (value === "queued" || value === "in_progress") return <ClockIcon />;
  return <XCircleFillIcon />;
}

function label(run: Pick<CheckRun, "status" | "conclusion">) {
  if (run.status === "queued") return "Queued";
  if (run.status === "in_progress") return "In progress";
  return (run.conclusion ?? "failure")
    .split("_")
    .map((part) => part[0].toUpperCase() + part.slice(1))
    .join(" ");
}

function timestamp(value: number | null) {
  if (value === null) return null;
  return new Date(value).toLocaleString(undefined, {
    dateStyle: "medium",
    timeStyle: "short",
  });
}

export function CheckRuns({
  repo,
  pull,
  oid,
  baseOid,
  selected,
  before,
  scopeToPull,
  githubActionsRef,
  candidate = false,
  live = false,
  scanHref,
}: {
  repo: Repository;
  oid: string;
  baseOid?: string;
  selected?: number;
  before?: number;
  scopeToPull?: boolean;
  githubActionsRef?: string | null;
  candidate?: boolean;
  live?: boolean;
} & (
  | { pull: number; scanHref?: never }
  | { pull?: never; scanHref: (filters?: Record<string, string>) => string }
)) {
  const scope = scopeToPull && pull !== undefined ? String(pull) : undefined;
  const page = useRequest<CheckPage>(
    endpoint(repo, `commits/${oid}/check-runs`, {
      limit: "50",
      before: before ? String(before) : undefined,
      pull: scope,
      scans_only: scanHref ? "true" : undefined,
    }),
    repo.id,
    undefined,
    live && !before
      ? {
          interval: ({ items }) =>
            items.some((run) => run.status !== "completed") ? 5_000 : 15_000,
        }
      : undefined,
  );
  const active = page.data && (selected ?? page.data.items[0]?.id);
  const activeRun = page.data?.items.find((run) => run.id === active);
  const baseline =
    activeRun && !activeRun.name.startsWith("Code scanning / ")
      ? undefined
      : baseOid;
  const detail = useRequest<CheckRun>(
    active
      ? endpoint(repo, `commits/${oid}/check-runs/${active}`, {
          pull: scope,
          baseline,
          scans_only: scanHref ? "true" : undefined,
        })
      : null,
    repo.id,
    undefined,
    live
      ? {
          revision: activeRun?.version,
          interval: baseline ? () => 15_000 : undefined,
        }
      : undefined,
  );
  const checkHref =
    scanHref ??
    ((filters: Record<string, string> = {}) =>
      repoHref(repo, {
        view: "pulls",
        pull: String(pull),
        pull_tab: "checks",
        candidate: candidate ? "1" : undefined,
        ...filters,
      }));
  return (
    <section
      className={
        githubActionsRef ? "check-runs grid-rows-[auto_1fr]" : "check-runs"
      }
      aria-label={scanHref ? "Scan reports" : "Checks"}
    >
      {githubActionsRef && (
        <MirrorRefspec key={oid} refspec={`${oid}:${githubActionsRef}`} />
      )}
      <Result state={page} timingLabel="Check runs">
        {(data) =>
          data.items.length ? (
            <>
              <aside
                className="check-run-list"
                aria-label={scanHref ? "Reports for this commit" : "Check runs"}
              >
                <header>
                  <strong>
                    {scanHref ? "Reports for this commit" : "All checks"}
                  </strong>
                  <code>{short(data.sha)}</code>
                </header>
                <nav>
                  {data.items.map((run) => (
                    <Link
                      key={run.id}
                      className={`${active === run.id ? "active" : ""} ${outcome(run)}`}
                      aria-current={active === run.id ? "page" : undefined}
                      href={checkHref({ check: String(run.id) })}
                    >
                      <OutcomeIcon {...run} />
                      <span>
                        <strong>{run.name}</strong>
                        <small>{label(run)}</small>
                        {run.provider === "github_actions" && (
                          <small>GitHub Actions</small>
                        )}
                      </span>
                    </Link>
                  ))}
                </nav>
                {(before || data.next) && (
                  <footer>
                    {before && (
                      <Link href={checkHref()}>
                        {scanHref ? "Newest reports" : "Newest checks"}
                      </Link>
                    )}
                    {data.next && (
                      <Link
                        href={checkHref({ check_before: String(data.next) })}
                      >
                        {scanHref ? "Older reports" : "Older checks"}
                      </Link>
                    )}
                  </footer>
                )}
              </aside>
              <div className="check-run-detail">
                <Result state={detail} timingLabel="Check details">
                  {(run) => (
                    <CheckRunDetail
                      run={run}
                      sourceRepo={scopeToPull || candidate ? undefined : repo}
                    />
                  )}
                </Result>
              </div>
            </>
          ) : (
            <div className="notice check-empty">
              <ClockIcon size={32} />
              <h3>
                {scanHref
                  ? "No scan reports for this commit"
                  : "No checks have been reported"}
              </h3>
              {!scanHref && (
                <p>CI check runs for this commit will appear here.</p>
              )}
            </div>
          )
        }
      </Result>
    </section>
  );
}

function MirrorRefspec({ refspec }: { refspec: string }) {
  const [message, setMessage] = useState("");
  return (
    <div className="col-span-full grid gap-2 border-b p-4">
      <label className="text-sm font-medium" htmlFor="github-actions-refspec">
        GitHub Actions mirror refspec
      </label>
      <p className="text-sm text-muted-foreground">
        Push this commit to the connected GitHub repository with this refspec to
        run its workflow.
      </p>
      <div className="flex gap-2">
        <Input
          id="github-actions-refspec"
          value={refspec}
          readOnly
          className="font-mono"
        />
        <Button
          type="button"
          variant="outline"
          size="icon"
          aria-label="Copy GitHub Actions mirror refspec"
          onClick={async () => {
            try {
              await navigator.clipboard.writeText(refspec);
              setMessage("Refspec copied.");
            } catch {
              setMessage("Select the refspec to copy it manually.");
            }
          }}
        >
          <CopyIcon />
        </Button>
      </div>
      {message && (
        <p role="status" className="text-sm">
          {message}
        </p>
      )}
    </div>
  );
}

function CheckRunDetail({
  run,
  sourceRepo,
}: {
  run: CheckRun;
  sourceRepo?: Repository;
}) {
  const output = run.output;
  if (!output) return null;
  const comparison = run.scan_comparison;
  return (
    <article className="check-run-content">
      <header className={`check-run-heading ${outcome(run)}`}>
        <OutcomeIcon {...run} />
        <div>
          <h3>{run.name}</h3>
          {run.provider === "github_actions" && (
            <p className="text-sm font-medium">Provider · GitHub Actions</p>
          )}
          <p className="muted">
            {label(run)} for <code>{short(run.head_sha)}</code> · reported by{" "}
            {run.author}
            {run.completed_at && ` · completed ${timestamp(run.completed_at)}`}
          </p>
        </div>
        {run.details_url && (
          <a href={run.details_url} target="_blank" rel="noreferrer">
            External details
          </a>
        )}
      </header>
      {comparison && (
        <section
          aria-label="Scan comparison"
          className="grid gap-1 rounded-md border p-3 text-sm"
        >
          <p>
            Compared with base{" "}
            <code title={comparison.baseline_sha}>
              {short(comparison.baseline_sha)}
            </code>
          </p>
          <p className="text-muted-foreground">
            {comparison.state === "ready"
              ? `${comparison.introduced} new · ${comparison.existing} existing · ${comparison.unclassified} unclassified`
              : comparisonMessages[comparison.state]}
          </p>
        </section>
      )}
      {output.annotations.length > 0 && (
        <section className="check-annotations" aria-label="Annotations">
          <h4>Annotations</h4>
          <ul>
            {output.annotations.map((annotation, index) => {
              const finding =
                comparison?.state === "ready"
                  ? comparison.annotations[index]
                  : undefined;
              const severity = {
                failure: "Error",
                warning: "Warning",
                notice: "Notice",
              }[annotation.level];
              const location =
                `${annotation.path}:${annotation.start_line}` +
                (annotation.end_line === annotation.start_line
                  ? ""
                  : `–${annotation.end_line}`);
              const href =
                sourceRepo &&
                sourceLocation(sourceRepo, run.head_sha, annotation);
              return (
                <li
                  className={annotation.level}
                  key={`${annotation.path}:${annotation.start_line}:${index}`}
                >
                  <DotFillIcon aria-hidden="true" />
                  <div>
                    <strong className="wrap-anywhere">
                      {severity}
                      {annotation.title && ` · ${annotation.title}`}
                    </strong>
                    {finding && (
                      <span className="text-xs text-muted-foreground">
                        {findingLabels[finding]}
                      </span>
                    )}
                    {href ? (
                      <Link className="wrap-anywhere underline" href={href}>
                        <code>{location}</code>
                      </Link>
                    ) : (
                      <code className="wrap-anywhere">{location}</code>
                    )}
                    <p className="whitespace-pre-wrap">{annotation.message}</p>
                  </div>
                </li>
              );
            })}
          </ul>
        </section>
      )}
      <section className="check-output panel">
        <h3>{output.title}</h3>
        <DiscussionMarkdown>{output.summary}</DiscussionMarkdown>
        {output.text && <DiscussionMarkdown>{output.text}</DiscussionMarkdown>}
      </section>
      <section className="check-steps panel" aria-label="Steps">
        <h3>Steps</h3>
        {output.steps.length ? (
          output.steps.map((step, index) => (
            <Disclosure
              key={`${step.name}:${index}`}
              summaryClassName={outcome(step)}
              summary={
                <>
                  <OutcomeIcon {...step} />
                  <strong>{step.name}</strong>
                  <span>{label(step)}</span>
                </>
              }
            >
              <pre>
                <code>{step.log || "No log output was reported."}</code>
              </pre>
            </Disclosure>
          ))
        ) : (
          <p className="muted">No steps were reported.</p>
        )}
      </section>
    </article>
  );
}

function sourceLocation(
  repo: Repository,
  sha: string,
  annotation: CheckAnnotation,
) {
  const path = annotation.path;
  if (
    path.includes("\\") ||
    path.split("/").some((part) => !part || part === "." || part === "..")
  )
    return undefined;
  const hex = Array.from(new TextEncoder().encode(path), (byte) =>
    byte.toString(16).padStart(2, "0"),
  ).join("");
  const href = repoHref(repo, {
    rev: sha,
    path: hex,
    kind: "Blob",
    repository_id: repo.id,
  });
  return `${href}#L${annotation.start_line}`;
}
