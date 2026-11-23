import type { MergeMethod } from "./model";

const presentation = {
  merge_commit: {
    value: "merge_commit",
    label: "Create a merge commit",
    action: "Merge pull request",
    description: "Add all commits with a two-parent merge commit.",
    completion: "created merge commit",
    pending: "a merge commit",
  },
  squash: {
    value: "squash",
    label: "Squash and merge",
    action: "Squash and merge",
    description: "Combine all pull request changes into one commit.",
    completion: "squashed changes as commit",
    pending: "a squash merge",
  },
  rebase: {
    value: "rebase",
    label: "Rebase and merge",
    action: "Rebase and merge",
    description: "Replay each commit onto the base branch in order.",
    completion: "rebased changes through commit",
    pending: "a rebase merge",
  },
  fast_forward: {
    value: "fast_forward",
    label: "Fast-forward only",
    action: "Merge pull request",
    description: "Move the base branch when it is an ancestor of the head.",
    completion: "fast-forwarded to commit",
    pending: "a fast-forward merge",
  },
} as const satisfies Record<
  MergeMethod,
  {
    value: MergeMethod;
    label: string;
    action: string;
    description: string;
    completion: string;
    pending: string;
  }
>;

export const mergeMethods = Object.values(presentation);

export function mergeMethod(method: MergeMethod) {
  return presentation[method];
}
