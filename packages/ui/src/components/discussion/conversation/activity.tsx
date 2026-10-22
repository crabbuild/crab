import { HistoryIcon } from "@primer/octicons-react";
import { Author } from "../author";
import { Link } from "@/components/navigation/link";
import { repoHref } from "@/lib/repository-route";
import type { Repository } from "@/lib/api";
import type { ActivityEvent, Target } from "./model";

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
    case "drafted":
      return "converted to draft";
    case "ready_for_review":
      return "marked ready for review";
    case "review_requested":
      return `requested a review from ${event.action.reviewer} on`;
    case "review_request_cancelled":
      return `cancelled the review request for ${event.action.reviewer} on`;
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
    case "issue_linked":
      return "linked issue";
    case "issue_unlinked":
      return "unlinked issue";
  }
}

export function ActivityItem({
  repo,
  event,
  href,
  hidden,
}: {
  repo: Repository;
  event: ActivityEvent;
  href?: string;
  hidden?: boolean;
}) {
  const parent = subject(event.target);
  const parameters = {
    view: parent.view,
    issue: parent.view === "issues" ? String(parent.number) : undefined,
    pull: parent.view === "pulls" ? String(parent.number) : undefined,
  };
  const parentHref = repoHref(repo, parameters);
  const date = new Date(event.created_at);
  const related = "number" in event.action ? event.action : undefined;
  const linkedIssue =
    related?.kind === "issue_linked" || related?.kind === "issue_unlinked";
  const relatedHref = related
    ? repoHref(repo, {
        view: linkedIssue ? "issues" : "milestones",
        issue: linkedIssue ? String(related.number) : undefined,
        milestone: linkedIssue ? undefined : String(related.number),
      })
    : undefined;

  return (
    <li
      id={`event-${event.number}`}
      tabIndex={-1}
      className="conversation-contribution conversation-activity"
      hidden={hidden}
      data-activity-number={event.number}
    >
      <HistoryIcon aria-hidden="true" />
      <div>
        <strong>
          <Author author={event.actor} />
        </strong>{" "}
        {action(event)}{" "}
        {related && (
          <>
            <Link href={relatedHref}>{related.title}</Link> on{" "}
          </>
        )}
        <Link href={parentHref}>
          #{parent.number} {event.title}
        </Link>
        <Link
          className="muted"
          href={href ?? parentHref}
          aria-label={`Event #${event.number} permalink`}
        >
          <time dateTime={date.toISOString()}>
            {date.toLocaleString(undefined, {
              dateStyle: "medium",
              timeStyle: "short",
            })}
          </time>
        </Link>
      </div>
    </li>
  );
}
