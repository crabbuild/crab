import { useEffect, useRef, useState, type ReactNode } from "react";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { useProjectCommands } from "./commands";
import {
  checkpointDraft,
  clearDraft,
  draftContent,
  draftKey,
  draftEvent,
  listDrafts,
  readDraft,
  type DraftData,
  type DraftScope,
} from "./draft-storage";

export function useProjectDraft<T extends DraftData>(
  scope: DraftScope,
  initial: T,
) {
  const { account } = useProjectCommands();
  const [state, setState] = useState(() => {
    try {
      const saved = account ? readDraft(account, scope, initial.kind) : null;
      return {
        data: (saved ?? initial) as T,
        restored: saved !== null,
        error: "",
        unreadable: false,
      };
    } catch {
      return {
        data: initial,
        restored: false,
        error:
          "Draft recovery is unavailable. The saved record has been retained. Allow browser storage and retry, or copy the record before discarding it.",
        unreadable: true,
      };
    }
  });
  const current = useRef(state);
  current.current = state;
  const initialValue = useRef(initial);
  initialValue.current = initial;
  useEffect(() => {
    const removed = (event: Event) => {
      const change = (
        event as CustomEvent<{
          removed?: boolean;
          key?: string;
          content?: string;
        }>
      ).detail;
      if (
        !account ||
        !change?.removed ||
        change.key !== draftKey(account, scope, initialValue.current.kind) ||
        change.content !== draftContent(account, scope, current.current.data)
      )
        return;
      const next = {
        data: initialValue.current,
        restored: false,
        error: "",
        unreadable: false,
      };
      current.current = next;
      setState(next);
    };
    window.addEventListener(draftEvent, removed);
    return () => window.removeEventListener(draftEvent, removed);
  }, [
    account,
    scope.project,
    scope.item,
    "repo" in scope.target ? scope.target.repo.id : scope.target.organization,
  ]);
  function update(patch: Partial<T>) {
    const next = {
      ...current.current,
      data: { ...current.current.data, ...patch },
    };
    if (account && !next.unreadable) {
      try {
        checkpointDraft(account, scope, next.data);
        next.error = "";
      } catch {
        next.error =
          "Your latest draft could not be saved in this tab. Keep this page open or copy your text, then allow/free browser storage and retry.";
      }
    }
    current.current = next;
    setState(next);
  }
  function checkpoint() {
    if (!account || current.current.unreadable) return null;
    try {
      return checkpointDraft(account, scope, current.current.data);
    } catch {
      const next = {
        ...current.current,
        error:
          "The draft could not be checkpointed. No submission was sent. Allow/free browser storage and retry.",
      };
      current.current = next;
      setState(next);
      return null;
    }
  }
  useEffect(() => {
    if (!state.error) return;
    const preserve = (event: BeforeUnloadEvent) => event.preventDefault();
    window.addEventListener("beforeunload", preserve);
    return () => window.removeEventListener("beforeunload", preserve);
  }, [state.error]);
  return {
    value: state.data,
    update,
    checkpoint,
    feedback: <DraftFeedback error={state.error} restored={state.restored} />,
  };
}
function DraftFeedback({
  error,
  restored,
}: {
  error: string;
  restored: boolean;
}) {
  return error ? (
    <p role="alert" className="error">
      {error}
    </p>
  ) : (
    <p className="text-xs text-muted-foreground">
      {restored ? "Draft restored, including its original revision. " : ""}Edits
      are saved in this tab for your account until submitted or discarded.
    </p>
  );
}
export function DraftRecovery({
  scope,
  disabled,
  onResume,
}: {
  scope: DraftScope;
  disabled?: boolean;
  onResume: (draft: { kind: DraftData["kind"]; item: string }) => void;
}) {
  const commands = useProjectCommands();
  const [, setGeneration] = useState(0);
  const [error, setError] = useState("");
  useEffect(() => {
    const changed = () => setGeneration((value) => value + 1);
    window.addEventListener(draftEvent, changed);
    return () => window.removeEventListener(draftEvent, changed);
  }, []);
  let content: ReactNode;
  try {
    const drafts = commands.account ? listDrafts(commands.account, scope) : [];
    if (!drafts.length && !error) return null;
    content = (
      <>
        <p>Unfinished drafts are saved in this tab.</p>
        <div className="flex flex-wrap gap-2">
          {drafts.map((draft) => (
            <div key={draft.key} className="flex flex-wrap gap-1">
              <Button
                variant="outline"
                size="sm"
                disabled={disabled || commands.locked}
                onClick={() => onResume(draft)}
              >
                Resume{" "}
                {draft.kind === "create"
                  ? "new project"
                  : draft.kind === "item"
                    ? "item"
                    : draft.kind}{" "}
                draft
              </Button>
              <Button asChild variant="ghost" size="sm">
                <a
                  download="crab-project-draft.json"
                  href={`data:application/json;charset=utf-8,${encodeURIComponent(sessionStorage.getItem(draft.key) ?? "")}`}
                >
                  Export {draft.kind} draft
                </a>
              </Button>
              <Button
                variant="ghost"
                size="sm"
                disabled={commands.locked}
                onClick={() => {
                  try {
                    const content = sessionStorage.getItem(draft.key);
                    if (content !== null)
                      clearDraft({ key: draft.key, content });
                    setError("");
                  } catch {
                    setError(
                      "The draft could not be discarded. Allow browser storage and retry.",
                    );
                  }
                }}
              >
                Discard {draft.kind} draft
              </Button>
            </div>
          ))}
        </div>
      </>
    );
  } catch {
    content = (
      <p>
        Draft recovery is unavailable. Allow browser storage, then reload to
        retry.
      </p>
    );
  }
  return (
    <Alert className="mb-4">
      <AlertDescription>
        {content}
        {error && <p role="alert">{error}</p>}
      </AlertDescription>
    </Alert>
  );
}
