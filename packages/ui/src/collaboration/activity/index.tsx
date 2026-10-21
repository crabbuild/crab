import { useEffect, useRef, useState } from "react";
import { Button, Spinner } from "@primer/react";
import { HistoryIcon } from "@primer/octicons-react";
import { endpoint, useRequest, type Repository } from "../../api";
import { repoHref } from "../../repository-route";
import { Link, RequestWidget } from "../../ui";

type Target =
  | { kind: "issue"; number: number }
  | { kind: "pull"; number: number }
  | { kind: "issue_comment"; issue: number; number: number }
  | {
      kind: "pull_comment" | "pull_review" | "pull_thread";
      pull: number;
      number: number;
    }
  | { kind: "pull_reply"; pull: number; thread: number; number: number };
type Action =
  | {
      kind:
        | "created"
        | "updated"
        | "closed"
        | "reopened"
        | "approved"
        | "changes_requested"
        | "commented"
        | "merged"
        | "resolved"
        | "unresolved";
    }
  | { kind: "milestone_assigned"; number: number; title: string }
  | { kind: "milestone_removed"; number: number; title: string };
export interface ActivityEvent {
  number: number;
  target: Target;
  title: string;
  actor: string;
  action: Action;
  created_at: number;
}
interface Page {
  items: ActivityEvent[];
  next: number | null;
}

function subject(target: Target) {
  if (target.kind === "issue") return { view: "issues", number: target.number };
  if (target.kind === "issue_comment")
    return { view: "issues", number: target.issue };
  return {
    view: "pulls",
    number: target.kind === "pull" ? target.number : target.pull,
  };
}

function action(event: ActivityEvent): string {
  switch (event.action.kind) {
    case "created":
      switch (event.target.kind) {
        case "issue":
          return "opened issue";
        case "pull":
          return "opened pull request";
        case "issue_comment":
        case "pull_comment":
          return "commented on";
        case "pull_thread":
          return "started a review conversation on";
        case "pull_reply":
          return "replied to a review conversation on";
        case "pull_review":
          return "reviewed";
      }
      break;
    case "updated":
      return "updated";
    case "closed":
      return "closed";
    case "reopened":
      return "reopened";
    case "approved":
      return "approved";
    case "changes_requested":
      return "requested changes to";
    case "commented":
      return "reviewed";
    case "merged":
      return "merged";
    case "resolved":
      return "resolved a review conversation on";
    case "unresolved":
      return "reopened a review conversation on";
    case "milestone_assigned":
      return "added milestone";
    case "milestone_removed":
      return "removed milestone";
  }
}

export default function RepositoryActivity({ repo }: { repo: Repository }) {
  return (
    <section className="activity-page">
      <div className="section-heading">
        <h2>Activity</h2>
        <Link href={repoHref(repo)}>Back to code</Link>
      </div>
      <ActivityTimeline repo={repo} />
    </section>
  );
}

export function ActivityTimeline({
  repo,
  target,
}: {
  repo: Repository;
  target?: string;
}) {
  const [before, setBefore] = useState<number>();
  const [items, setItems] = useState<ActivityEvent[]>([]);
  const [next, setNext] = useState<number | null>(null);
  const heading = useRef<HTMLHeadingElement>(null);
  const resource = useRequest<Page>(
    endpoint(repo, target ? `${target}/activity` : "activity", {
      before: before?.toString(),
      limit: "30",
    }),
  );
  useEffect(() => {
    if (!resource.data) return;
    const page = resource.data;
    setItems((previous) => {
      const events = new Map(
        (before === undefined ? [] : previous).map((event) => [
          event.number,
          event,
        ]),
      );
      for (const event of page.items) events.set(event.number, event);
      return [...events.values()].sort((a, b) => b.number - a.number);
    });
    setNext(page.next);
    if (before !== undefined && page.next === null) heading.current?.focus();
  }, [before, resource.data]);
  function refresh() {
    setBefore(undefined);
    resource.retry();
  }
  return (
    <RequestWidget timing={resource.timing} label="Activity">
      <section className="panel activity-panel" aria-label="Activity">
        <div className="section-heading">
          <h3 ref={heading} tabIndex={-1}>
            Recent activity
          </h3>
          <Button size="small" disabled={resource.loading} onClick={refresh}>
            Refresh activity
          </Button>
        </div>
        {resource.loading && (
          <p className="muted" role="status">
            <Spinner size="small" /> Loading activity…
          </p>
        )}
        {resource.error && (
          <div className="notice error" role="alert">
            <p>{resource.error}</p>
            <Button size="small" onClick={resource.retry}>
              Retry activity
            </Button>
          </div>
        )}
        {!resource.loading && !resource.error && !items.length && (
          <p className="muted">
            No activity recorded yet. New changes will appear here.
          </p>
        )}
        <ol className="activity-events" aria-label="Activity, newest first">
          {items.map((event) => {
            const parent = subject(event.target);
            const parameters = {
              view: parent.view,
              issue:
                parent.view === "issues" ? String(parent.number) : undefined,
              pull: parent.view === "pulls" ? String(parent.number) : undefined,
            };
            const href = repoHref(repo, parameters);
            const date = new Date(event.created_at);
            const milestone =
              event.action.kind === "milestone_assigned" ||
              event.action.kind === "milestone_removed"
                ? event.action
                : undefined;
            const milestoneHref = milestone
              ? repoHref(repo, {
                  view: "milestones",
                  milestone: String(milestone.number),
                })
              : undefined;
            return (
              <li key={event.number} data-activity-number={event.number}>
                <HistoryIcon aria-hidden="true" />
                <div>
                  <strong>{event.actor}</strong> {action(event)}{" "}
                  {milestone && (
                    <>
                      <Link href={milestoneHref}>{milestone.title}</Link>{" "}
                      on{" "}
                    </>
                  )}
                  <Link href={href}>
                    #{parent.number} {event.title}
                  </Link>
                  <time dateTime={date.toISOString()}>
                    {date.toLocaleString(undefined, {
                      dateStyle: "medium",
                      timeStyle: "short",
                    })}
                  </time>
                </div>
              </li>
            );
          })}
        </ol>
        {next !== null && (
          <Button
            size="small"
            disabled={resource.loading}
            onClick={() => setBefore(next)}
          >
            Load older activity
          </Button>
        )}
      </section>
    </RequestWidget>
  );
}
