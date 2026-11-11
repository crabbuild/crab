import { useMemo } from "react";
import { mainProcess, rpc } from "../ipc";
import { type OpenRepo, useStore } from "../state";
import { formatBytes } from "../lib/format";
import { requiresTransferConfirmation, resolveScope, type HydrateVerb, type Scope } from "../lib/hydrateScope";
import { fileBuffers } from "../lib/file-buffer-store";
import { fileBufferKey } from "./useFileBuffer";

export type SelectionTransfers = Partial<Record<HydrateVerb, (scope?: Scope) => Promise<number | null>>> & {
  restore?: (paths?: string[]) => Promise<number | null>;
};

/** Resolve UI scopes to literal paths before starting repository-owned work. */
export function useFileTransfers(repo: OpenRepo | null, enabled = true): SelectionTransfers {
  const { runFileTransfer } = useStore();
  const repoId = repo?.repo_id;
  const session = repo?.native_session_id;
  const hydrate = enabled && !repo?.deferred && !!session && !!repo?.capabilities?.canHydrateSelection;
  const dehydrate = enabled && !repo?.deferred && !!session && !!repo?.capabilities?.canDehydrateSelection;
  const restore = enabled && !repo?.deferred && !!session && !!repo?.capabilities?.canRestoreSelection;

  return useMemo(() => {
    const run = (verb: HydrateVerb) => async (scope: Scope = { kind: "all" }) => {
      if (!repoId || !session) throw new Error("Repository services are unavailable");
      const inventory = await rpc.hydrationStatus(repoId);
      const { matched, bytes } = resolveScope(scope, inventory.items, verb);
      if (matched.length === 0) return 0;
      if (requiresTransferConfirmation(verb, matched.length, bytes)) {
        const action = verb === "hydrate" ? "Hydrate" : "Dehydrate";
        const impact = verb === "hydrate" ? "Writes verified content to disk." : "Verifies saved content before replacing working files with pointers.";
        if (!await mainProcess.dialogConfirm(`${action} ${matched.length} ${matched.length === 1 ? "file" : "files"} (${formatBytes(bytes)})? ${impact}`)) return null;
      }
      // Keep the session captured before inventory/confirmation. Rebinding or
      // recovery must reject this intent rather than write through a new owner.
      await runFileTransfer(repoId, { verb, paths: matched.map(entry => entry.path) }, session);
      return matched.length;
    };
    const restoreFiles = async (paths?: string[]) => {
      if (!repo || !repoId || !session) throw new Error("Repository services are unavailable");
      const status = await rpc.gitStatus(repoId);
      // Porcelain XY keeps staging in X and unstaged changes in Y. Restoring
      // the index must neither erase staged-only files nor remove untracked work.
      const eligible = status.entries.filter(entry => entry.code.length === 2 && entry.code !== "??" && entry.code[1] !== " ");
      const requested = paths ? new Set(paths) : null;
      const selected = [...new Set(eligible.filter(entry => !requested || requested.has(entry.path)).map(entry => entry.path))];
      if (!selected.length) return 0;
      const release = fileBuffers.protect(selected.map(path => fileBufferKey(repo, path)));
      try {
        if (!await mainProcess.dialogConfirm(`Restore ${selected.length} ${selected.length === 1 ? "file" : "files"} from the index? This permanently discards their unstaged changes. Staged changes and untracked files are preserved. This cannot be undone.`)) return null;
        await runFileTransfer(repoId, { verb: "restore", paths: selected }, session);
        return selected.length;
      } finally { release(); }
    };
    return { hydrate: hydrate ? run("hydrate") : undefined, dehydrate: dehydrate ? run("dehydrate") : undefined, restore: restore ? restoreFiles : undefined };
  }, [repo, repoId, session, hydrate, dehydrate, restore, runFileTransfer]);
}
