import type { ReactionSummary } from "../../src/collaboration/reactions";

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
