import {
  GitMergeIcon,
  GitPullRequestClosedIcon,
  GitPullRequestDraftIcon,
  GitPullRequestIcon,
} from "@primer/octicons-react";

import type { PullRequest } from "./model";

export function branch(name: string) {
  return name.startsWith("refs/heads/")
    ? name.slice("refs/heads/".length)
    : name;
}

export const pullIcons = {
  open: GitPullRequestIcon,
  draft: GitPullRequestDraftIcon,
  closed: GitPullRequestClosedIcon,
  merged: GitMergeIcon,
};
export function pullStatus(pull: {
  state: PullRequest["state"];
  draft: boolean;
}) {
  return pull.state === "open" && pull.draft ? "draft" : pull.state;
}
