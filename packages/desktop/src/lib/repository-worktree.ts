import { parseRepoId } from "../../electron/ssh-types";
import type { OpenRepo } from "../state";

/** Return the physical workspace root; SDK snapshots have no mutable directory. */
export function repositoryWorktree(repo: Pick<OpenRepo, "repo_id" | "path" | "is_remote" | "deferred">): string | undefined {
  if (repo.deferred) return undefined;
  return parseRepoId(repo.repo_id)?.remotePath ?? (!repo.is_remote ? repo.path : undefined);
}
