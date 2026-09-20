import {
  CheckCircleFillIcon,
  ClockIcon,
  DotFillIcon,
  SkipIcon,
  XCircleFillIcon,
} from "@primer/octicons-react";
import { endpoint, repoHref, useRequest, type Repository } from "./api";
import { DiscussionMarkdown } from "./discussion";
import { Link, Result, short } from "./ui";

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
  version: number;
  started_at: number | null;
  completed_at: number | null;
  created_at: number;
  updated_at: number;
  output?: CheckOutput;
}

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
  selected,
  before,
}: {
  repo: Repository;
  pull: number;
  oid: string;
  selected?: number;
  before?: number;
}) {
  const page = useRequest<CheckPage>(
    endpoint(repo, `commits/${oid}/check-runs`, {
      limit: "50",
      before: before ? String(before) : undefined,
    }),
  );
  const active = selected ?? page.data?.items[0]?.id;
  const detail = useRequest<CheckRun>(
    active ? endpoint(repo, `commits/${oid}/check-runs/${active}`) : null,
  );
  return (
    <section className="check-runs" aria-label="Checks">
      <Result state={page} showTiming={false}>
        {(data) =>
          data.items.length ? (
            <>
              <aside className="check-run-list" aria-label="Check runs">
                <header>
                  <strong>All checks</strong>
                  <code>{short(data.sha)}</code>
                </header>
                <nav>
                  {data.items.map((run) => (
                    <Link
                      key={run.id}
                      className={`${active === run.id ? "active" : ""} ${outcome(run)}`}
                      aria-current={active === run.id ? "page" : undefined}
                      href={repoHref(repo, {
                        view: "pulls",
                        pull: String(pull),
                        pull_tab: "checks",
                        check: String(run.id),
                      })}
                    >
                      <OutcomeIcon {...run} />
                      <span>
                        <strong>{run.name}</strong>
                        <small>{label(run)}</small>
                      </span>
                    </Link>
                  ))}
                </nav>
                {(before || data.next) && (
                  <footer>
                    {before && (
                      <Link
                        href={repoHref(repo, {
                          view: "pulls",
                          pull: String(pull),
                          pull_tab: "checks",
                        })}
                      >
                        Newest checks
                      </Link>
                    )}
                    {data.next && (
                      <Link
                        href={repoHref(repo, {
                          view: "pulls",
                          pull: String(pull),
                          pull_tab: "checks",
                          check_before: String(data.next),
                        })}
                      >
                        Older checks
                      </Link>
                    )}
                  </footer>
                )}
              </aside>
              <div className="check-run-detail">
                <Result state={detail}>
                  {(run) => <CheckRunDetail run={run} />}
                </Result>
              </div>
            </>
          ) : (
            <div className="notice check-empty">
              <ClockIcon size={32} />
              <h3>No checks have been reported</h3>
              <p>CI check runs for this commit will appear here.</p>
            </div>
          )
        }
      </Result>
    </section>
  );
}

function CheckRunDetail({ run }: { run: CheckRun }) {
  const output = run.output;
  if (!output) return null;
  return (
    <article className="check-run-content">
      <header className={`check-run-heading ${outcome(run)}`}>
        <OutcomeIcon {...run} />
        <div>
          <h3>{run.name}</h3>
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
      {output.annotations.length > 0 && (
        <section className="check-annotations" aria-label="Annotations">
          <h4>Annotations</h4>
          <ul>
            {output.annotations.map((annotation, index) => (
              <li
                className={annotation.level}
                key={`${annotation.path}:${annotation.start_line}:${index}`}
              >
                <DotFillIcon />
                <div>
                  <strong>
                    {annotation.title ?? annotation.message.split("\n")[0]}
                  </strong>
                  <code>
                    {annotation.path}:{annotation.start_line}
                    {annotation.end_line !== annotation.start_line &&
                      `–${annotation.end_line}`}
                  </code>
                  {annotation.title && <p>{annotation.message}</p>}
                </div>
              </li>
            ))}
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
            <details key={`${step.name}:${index}`}>
              <summary className={outcome(step)}>
                <OutcomeIcon {...step} />
                <strong>{step.name}</strong>
                <span>{label(step)}</span>
              </summary>
              <pre>
                <code>{step.log || "No log output was reported."}</code>
              </pre>
            </details>
          ))
        ) : (
          <p className="muted">No steps were reported.</p>
        )}
      </section>
    </article>
  );
}
