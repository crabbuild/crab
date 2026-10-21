import { IssueClosedIcon, IssueOpenedIcon } from "@primer/octicons-react";
import { Link } from "@/components/navigation/link";
import { LabelBadges } from "@/features/labels/selection";
import type { IssueListItem } from "@/features/issues/model";
import type { PullListItem } from "@/features/pull-requests/model";
import {
  branch,
  pullIcons,
  pullStatus,
} from "@/features/pull-requests/presentation";
import type { Repository } from "@/lib/api";
import { repoHref } from "@/lib/repository-route";
import { DiscussionCommentCount } from "./list";
import { AssigneeAvatars } from "./metadata";
import { timestamp } from "./time";

export type DiscussionItem = IssueListItem | PullListItem;

export function DiscussionRow({
  item,
  repo,
}: {
  item: DiscussionItem;
  repo: Repository;
}) {
  const status = item.kind === "pull" ? pullStatus(item) : item.state;
  const Icon =
    item.kind === "pull"
      ? pullIcons[status]
      : item.state === "open"
        ? IssueOpenedIcon
        : IssueClosedIcon;
  const href = repoHref(
    repo,
    item.kind === "pull"
      ? { view: "pulls", pull: String(item.number) }
      : { view: "issues", issue: String(item.number) },
  );
  return (
    <li>
      <span
        className={`${item.kind === "pull" ? "pull" : "issue"}-status-icon ${status}`}
        role="img"
        aria-label={status}
      >
        <Icon />
      </span>
      <div>
        <Link className="issue-link" href={href}>
          {item.title}
        </Link>
        <LabelBadges labels={item.labels} />
        <p className="muted">
          #{item.number} opened {timestamp(item.created_at)} by {item.author}
        </p>
        {item.kind === "pull" && (
          <p className="pull-branches muted">
            <code>{branch(item.head_ref)}</code>
            <span>into</span>
            <code>{branch(item.base_ref)}</code>
          </p>
        )}
      </div>
      <AssigneeAvatars assignees={item.assignees} />
      <DiscussionCommentCount count={item.comment_count} href={href} />
    </li>
  );
}
