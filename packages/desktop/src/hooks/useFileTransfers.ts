import { useMemo } from "react";
import { mainProcess, rpc } from "../ipc";
import { type OpenRepo, useStore } from "../state";
import { formatBytes } from "../lib/format";
import { requiresTransferConfirmation, resolveScope, type HydrateVerb, type Scope } from "../lib/hydrateScope";

export type SelectionTransfers = Partial<Record<HydrateVerb, (scope?: Scope) => Promise<number | null>>>;

/** Resolve UI scopes to literal paths before starting repository-owned work. */
export function useFileTransfers(repo: OpenRepo | null, enabled = true): SelectionTransfers {
  const { runFileTransfer } = useStore();
  const repoId = repo?.repo_id;
  const session = repo?.native_session_id;
  const hydrate = enabled && !repo?.deferred && !!session && !!repo?.capabilities?.canHydrateSelection;
  const dehydrate = enabled && !repo?.deferred && !!session && !!repo?.capabilities?.canDehydrateSelection;

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
    return { hydrate: hydrate ? run("hydrate") : undefined, dehydrate: dehydrate ? run("dehydrate") : undefined };
  }, [repoId, session, hydrate, dehydrate, runFileTransfer]);
}
