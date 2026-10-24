import { HistoryIcon } from "@primer/octicons-react";
import { PinIcon, PinOffIcon } from "lucide-react";
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
    case "project_changed":
      return "updated planning in project";
    case "project_removed":
      return "removed from project";
    case "parent_assigned":
      return "set parent issue";
    case "parent_removed":
      return "removed parent issue";
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
    case "issue_closed":
      return event.action.reason === "not_planned"
        ? "closed as not planned"
        : "closed as completed";
    case "issue_close_reason_changed":
      return `changed the close reason to ${event.action.to === "not_planned" ? "not planned" : "completed"} on`;
    case "closed":
      return "closed";
    case "reopened":
      return "reopened";
    case "pinned":
      return "pinned";
    case "unpinned":
      return "unpinned";
    case "approved":
      return "approved";
    case "review_dismissed":
      return "dismissed a review on";
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
  const Icon =
    event.action.kind === "pinned"
      ? PinIcon
      : event.action.kind === "unpinned"
        ? PinOffIcon
        : HistoryIcon;
  const related =
    "number" in event.action || "project" in event.action
      ? event.action
      : undefined;
  const linkedIssue =
    related?.kind === "issue_linked" ||
    related?.kind === "issue_unlinked" ||
    related?.kind === "parent_assigned" ||
    related?.kind === "parent_removed";
  const relatedHref =
    related && "project" in related
      ? repoHref(repo, { view: "projects", project: related.project })
      : related
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
      <Icon aria-hidden="true" />
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
        <Link
          href={
            event.action.kind === "review_dismissed" &&
            event.target.kind === "pull_review"
              ? `${parentHref}#pullrequestreview-${event.target.number}`
              : parentHref
          }
        >
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
