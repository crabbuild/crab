import {
  AtSignIcon,
  BellIcon,
  GitPullRequestIcon,
  MoveRightIcon,
  TagIcon,
  UserRoundIcon,
} from "lucide-react";

export const reasons = {
  release: { label: "Releases", summary: "Release published", icon: TagIcon },
  subscribed: {
    label: "Watched updates",
    summary: "Watched discussion",
    icon: BellIcon,
  },
  assigned: {
    label: "Assignments",
    summary: "Assignment",
    icon: UserRoundIcon,
  },
  review_requested: {
    label: "Review requests",
    summary: "Review request",
    icon: GitPullRequestIcon,
  },
  mention: { label: "Mentions", summary: "Mention", icon: AtSignIcon },
  team_mention: {
    label: "Team mentions",
    summary: "Team mention",
    icon: AtSignIcon,
  },
  issue_transferred: {
    label: "Issue transfers",
    summary: "Issue transferred",
    icon: MoveRightIcon,
  },
};

export const views = {
  unread: { label: "Unread", empty: "No unread notifications." },
  all: { label: "All", empty: "No notifications yet." },
  read: { label: "Read", empty: "No read notifications." },
  saved: { label: "Saved", empty: "No saved notifications." },
  done: { label: "Done", empty: "No completed notifications." },
};

export const kinds = {
  issue: "Issues",
  pull: "Pull requests",
  release: "Releases",
};

export type Reason = keyof typeof reasons;
export type View = keyof typeof views;
export type Criteria = {
  view: View;
  repository?: string;
  organization?: string;
  reason?: Reason;
  kind?: keyof typeof kinds;
};

function choice<T extends string>(
  options: Record<T, unknown>,
  value: string | null,
) {
  return value && Object.hasOwn(options, value) ? (value as T) : undefined;
}

export function isReason(value: string | null): value is Reason {
  return choice(reasons, value) !== undefined;
}

export function criteriaAt(url: URL): Criteria {
  const params = url.searchParams;
  return {
    view: choice(views, params.get("tab")) ?? "unread",
    repository: params.get("repository") || undefined,
    organization: params.get("organization") || undefined,
    reason: choice(reasons, params.get("reason")),
    kind: choice(kinds, params.get("kind")),
  };
}

function qualifiers(criteria: Criteria) {
  const query = new URLSearchParams();
  if (criteria.repository) query.set("repository", criteria.repository);
  if (criteria.organization) query.set("organization", criteria.organization);
  if (criteria.reason) query.set("reason", criteria.reason);
  if (criteria.kind) query.set("kind", criteria.kind);
  return query;
}

export function inboxHref(criteria: Criteria, preset?: string) {
  const query = qualifiers(criteria);
  if (criteria.view !== "unread") query.set("tab", criteria.view);
  if (preset) query.set("preset", preset);
  return `/-/notifications${query.size ? `?${query}` : ""}`;
}

export function inboxQuery(criteria: Criteria) {
  const query = qualifiers(criteria);
  query.set("unread", String(criteria.view === "unread"));
  if (criteria.view === "read") query.set("read", "true");
  if (criteria.view === "saved" || criteria.view === "done")
    query.set("view", criteria.view);
  return query;
}
