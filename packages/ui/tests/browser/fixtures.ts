import type { ReactionSummary } from "@/components/discussion/reactions";

export function emptyReactions(revision = 1): ReactionSummary {
  return {
    revision,
    groups: {
      thumbs_up: { count: 0, reacted: false },
      thumbs_down: { count: 0, reacted: false },
      laugh: { count: 0, reacted: false },
      hooray: { count: 0, reacted: false },
      confused: { count: 0, reacted: false },
      heart: { count: 0, reacted: false },
      rocket: { count: 0, reacted: false },
      eyes: { count: 0, reacted: false },
    },
  };
}

export function emptyReviewers() {
  return {
    items: [],
    next: null,
    version: 1,
    can_manage: false,
    candidates: [],
  };
}

export function emptyPullRollup(
  number = 1,
  head: string | null = "b".repeat(40),
): import("@/features/pull-requests/rollups").PullRollup {
  return {
    number,
    head_oid: head,
    checks: head
      ? { success: 0, neutral: 0, skipped: 0, pending: 0, failed: 0 }
      : null,
    reviews: head
      ? { approved: 0, changes_requested: 0, requested: 0, outdated: 0 }
      : null,
  };
}
