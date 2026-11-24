import {
  createContext,
  Fragment,
  useContext,
  useEffect,
  useCallback,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import {
  projectEndpoint,
  type BulkCommand,
  type BulkEdit,
  type Command,
  type Outcome,
  type ProjectRepository,
} from "./model";
import type { WorkspaceCommand } from "./workspace/model";
import { clearDraft, type DraftCheckpoint } from "./draft-storage";
import {
  readProjectIntent,
  removeProjectIntent,
  saveProjectIntent,
  type ProjectIntent as Intent,
} from "./journal";
interface Commands {
  account: string | null;
  csrf: string;
  submit: (
    repo: ProjectRepository,
    command: Command,
    draft?: DraftCheckpoint,
  ) => Promise<Outcome | undefined>;
  submitOrganization: (
    organization: string,
    command: WorkspaceCommand,
    draft?: DraftCheckpoint,
  ) => Promise<Outcome | undefined>;
  submitBulk: (
    target: Intent["target"],
    command: BulkCommand,
    undo: BulkEdit[],
  ) => Promise<Outcome | undefined>;
  locked: boolean;
  generation: number;
  error?: string;
  clearError: () => void;
  pending: boolean;
  canRecover: boolean;
  storageBlocked: boolean;
  retryStorage: () => void;
  recover: (cancel: boolean) => Promise<Outcome | undefined>;
}
const Context = createContext<Commands | null>(null);
export function useProjectCommands() {
  const value = useContext(Context);
  if (!value) throw new Error("Project command provider is missing");
  return value;
}
export function ProjectCommandsProvider({
  csrf,
  account,
  children,
}: {
  csrf: string;
  account: string | null;
  children: ReactNode;
}) {
  const [intent, setIntent] = useState<Intent | null>(null);
  const current = useRef<Intent | null>(null);
  const busy = useRef(false);
  const completion = useRef<((outcome: Outcome) => void) | null>(null);
  const [generation, setGeneration] = useState(0);
  const [error, setError] = useState<string>();
  const [notice, setNotice] = useState<{ title: string; message: string }>();
  const [undo, setUndo] = useState<{
    account: string;
    target: Intent["target"];
    command: BulkCommand;
  }>();
  const [pending, setPending] = useState(false);
  const [journalError, setJournalError] = useState<string>();
  const activeAccount = useRef(account);
  activeAccount.current = account;
  const retryStorage = useCallback(() => {
    if (!account || (current.current && current.current.account !== account))
      return;
    try {
      if (current.current) {
        saveProjectIntent(current.current);
        setJournalError(undefined);
        return;
      }
      const saved = readProjectIntent(account);
      setJournalError(undefined);
      if (!saved) return;
      current.current = saved;
      setIntent(saved);
      setError(
        "A project submission was interrupted. Recover its saved result before making more changes.",
      );
    } catch (failure) {
      setJournalError(
        failure instanceof Error
          ? failure.message
          : "Project recovery storage is unavailable.",
      );
    }
  }, [account]);
  useEffect(retryStorage, [retryStorage]);
  useEffect(() => {
    if (!intent) return;
    const preserve = (event: BeforeUnloadEvent) => event.preventDefault();
    window.addEventListener("beforeunload", preserve);
    return () => window.removeEventListener("beforeunload", preserve);
  }, [intent]);
  async function send(captured: Intent, cancel: boolean) {
    if (busy.current || activeAccount.current !== captured.account) return;
    busy.current = true;
    setPending(true);
    setError(undefined);
    setNotice(undefined);
    setUndo(undefined);
    try {
      // Recovery must use the persisted intent, including its original target.
      // Never turn a lost response into a replacement submission after reload.
      saveProjectIntent(captured);
      // The captured repository, account, UUID, and payload survive route changes.
      // An unproven response never releases the intent or permits a replacement write.
      const response = await fetch(
        projectEndpoint(captured.target, cancel ? "cancel" : "commands"),
        {
          method: "POST",
          headers: {
            "Content-Type": "application/json",
            "X-CSRF-Token": csrf,
            ...("repo" in captured.target
              ? { "X-Crab-Repository-Id": captured.target.repo.id }
              : {}),
          },
          body: JSON.stringify(captured.body),
          signal: AbortSignal.timeout(35000),
        },
      );
      if (response.status === 401)
        window.dispatchEvent(new Event("crab-session-expired"));
      const result: unknown = await response.json().catch(() => null);
      if (activeAccount.current !== captured.account) return;
      const rejected =
        !response.ok &&
        response.headers.get("X-Crab-Write-Outcome") === "rejected";
      if (!response.ok && !rejected)
        throw new Error(
          "The submission is unconfirmed. Retry to recover its saved result.",
        );
      const failure = result as { error?: { message?: string } } | null;
      const value: Outcome = rejected
        ? {
            outcome: "rejected",
            reason:
              typeof failure?.error?.message === "string"
                ? failure.error.message
                : "The project input was rejected. Check the field values and sizes.",
          }
        : (result as Outcome);
      if (value?.outcome === "rejected" && typeof value.reason !== "string")
        throw new Error("The server rejection was invalid.");
      if (
        !value ||
        (value.outcome !== "applied" &&
          value.outcome !== "issue_already_linked" &&
          value.outcome !== "cancelled" &&
          value.outcome !== "rejected" &&
          value.outcome !== "item_conflict")
      )
        throw new Error("The server acknowledgement was invalid.");
      if (value.outcome === "item_conflict") {
        const command = captured.body.command;
        const target =
          command.kind === "bulk_fields"
            ? command.items.some((edit) => edit.item === value.item)
            : command.kind === "item" ||
                command.kind === "remove" ||
                command.kind === "create_issue"
              ? value.item ===
                (typeof command.item === "string"
                  ? command.item
                  : command.item.id)
              : false;
        if (
          !target ||
          !("project" in command) ||
          value.project !== command.project ||
          !Number.isSafeInteger(value.version) ||
          value.version < 0
        )
          throw new Error("The conflict did not match this submission.");
      }
      const applied =
        value.outcome === "applied" || value.outcome === "issue_already_linked";
      if (applied) {
        const command = captured.body.command;
        const expected =
          command.kind === "parent" ||
          command.kind === "sub_issue" ||
          command.kind === "reorder_sub_issue" ||
          command.kind === "set_default_access"
            ? null
            : command.kind === "create" || command.kind === "copy"
              ? command.id
              : command.project;
        const addsExistingIssue =
          command.kind === "item" &&
          command.version === 0 &&
          command.item.subject?.kind === "issue";
        if (
          value.project !== expected ||
          (value.outcome === "issue_already_linked" &&
            (!("organization" in captured.target) ||
              (command.kind !== "create_issue" && !addsExistingIssue))) ||
          !Number.isSafeInteger(value.version) ||
          value.version < 0
        )
          throw new Error(
            "The server acknowledgement did not match this submission.",
          );
        setGeneration((previous) => previous + 1);
      }
      if (activeAccount.current !== captured.account) return;
      if (applied && captured.draft) clearDraft(captured.draft);
      removeProjectIntent(captured);
      current.current = null;
      setIntent(null);
      if (value.outcome === "rejected") setError(value.reason);
      if (value.outcome === "issue_already_linked")
        setNotice({
          title: "Issue added to Project",
          message:
            "The issue was already added to this Project. Its current planning fields were kept.",
        });
      if (
        value.outcome === "applied" &&
        captured.body.command.kind === "bulk_fields"
      ) {
        const command = captured.body.command;
        const count = captured.undo?.length ?? command.items.length;
        setNotice({
          title: "Project items updated",
          message: `Saved ${count} Project item${count === 1 ? "" : "s"}.`,
        });
        if (captured.undo?.length) {
          setUndo({
            account: captured.account,
            target: captured.target,
            command: { ...command, items: captured.undo },
          });
        }
      }
      if (value.outcome === "item_conflict")
        setError(
          captured.body.command.kind === "bulk_fields"
            ? "A selected item changed. Refresh the Project and select the items again."
            : "This item changed since you opened it. Your draft is preserved. Review the saved item before saving again.",
        );
      completion.current?.(value);
      completion.current = null;
      return value;
    } catch (failure) {
      setError(
        failure instanceof Error
          ? failure.message
          : "The response was lost. Retry this submission.",
      );
    } finally {
      busy.current = false;
      setPending(false);
    }
  }
  function submit(
    target: Intent["target"],
    command: Command | WorkspaceCommand,
    draft?: DraftCheckpoint,
    undo?: BulkEdit[],
  ) {
    if (!account || current.current || busy.current || journalError)
      return Promise.resolve(undefined);
    const captured: Intent = {
      account,
      target:
        "repo" in target
          ? {
              repo: {
                id: target.repo.id,
                owner: target.repo.owner,
                name: target.repo.name,
                archived: target.repo.archived,
              },
            }
          : target,
      body: { request_id: crypto.randomUUID(), command },
      ...(draft ? { draft } : {}),
      ...(undo ? { undo } : {}),
    };
    try {
      saveProjectIntent(captured);
    } catch (failure) {
      setJournalError(
        failure instanceof Error
          ? failure.message
          : "Project recovery storage is unavailable. No changes were sent.",
      );
      return Promise.resolve(undefined);
    }
    current.current = captured;
    setIntent(captured);
    return new Promise<Outcome | undefined>((resolve) => {
      completion.current = resolve;
      void send(captured, false);
    });
  }
  return (
    <Context
      value={{
        account,
        csrf,
        submit: (repo, command, draft) =>
          submit({ repo: { ...repo } }, command, draft),
        submitOrganization: (organization, command, draft) =>
          submit({ organization }, command, draft),
        submitBulk: (target, command, undo) =>
          submit(target, command, undefined, undo),
        locked: !!intent,
        generation,
        error: journalError ?? error,
        pending,
        clearError: () => setError(undefined),
        canRecover: !!intent && intent.account === account,
        storageBlocked: !!journalError,
        retryStorage,
        recover: (cancel) =>
          current.current
            ? send(current.current, cancel)
            : Promise.resolve(undefined),
      }}
    >
      {(intent || error || notice) && (
        <Alert className="project-feedback" aria-live="polite">
          <AlertTitle>
            {intent
              ? "Project submission"
              : error
                ? "Changes were not saved"
                : notice?.title}
          </AlertTitle>
          <AlertDescription>
            {intent?.account !== account && intent ? (
              <p>
                Sign in with the account that started this submission to recover
                it.
              </p>
            ) : (
              <>
                <p>{error ?? notice?.message ?? "Saving your changes…"}</p>
                {undo?.account === account && !intent && (
                  <Button
                    variant="outline"
                    size="sm"
                    disabled={pending}
                    onClick={() => void submit(undo.target, undo.command)}
                  >
                    Undo
                  </Button>
                )}
                {intent && (
                  <div className="flex flex-wrap gap-2">
                    <Button
                      variant="outline"
                      size="sm"
                      disabled={pending}
                      onClick={() => void send(intent, false)}
                    >
                      Retry submission
                    </Button>
                    <Button
                      variant="outline"
                      size="sm"
                      disabled={pending}
                      onClick={() => void send(intent, true)}
                    >
                      Cancel if not applied
                    </Button>
                  </div>
                )}
                {!intent && (
                  <Button
                    variant="ghost"
                    size="sm"
                    onClick={() => {
                      setError(undefined);
                      setNotice(undefined);
                      setUndo(undefined);
                    }}
                  >
                    Dismiss
                  </Button>
                )}
              </>
            )}
          </AlertDescription>
        </Alert>
      )}
      <Fragment key={account}>{children}</Fragment>
      {journalError && (
        <Alert variant="destructive" className="project-feedback" role="alert">
          <AlertTitle>Project recovery is unavailable</AlertTitle>
          <AlertDescription>
            <p>{journalError}</p>
            <p>
              No new changes will be sent until the saved session can be
              recovered.
            </p>
            <Button variant="outline" size="sm" onClick={retryStorage}>
              Retry recovery storage
            </Button>
          </AlertDescription>
        </Alert>
      )}
    </Context>
  );
}

export function CommandError() {
  const commands = useProjectCommands();
  if (!commands.error) return null;
  return (
    <div className="flex flex-col gap-3">
      <p role="alert" className="error">
        {commands.error}
      </p>
      {commands.storageBlocked ? (
        <Button type="button" variant="outline" onClick={commands.retryStorage}>
          Retry recovery storage
        </Button>
      ) : (
        commands.locked &&
        (commands.canRecover ? (
          <div className="flex flex-wrap gap-2">
            <Button
              type="button"
              variant="outline"
              disabled={commands.pending}
              onClick={() => void commands.recover(false)}
            >
              Retry submission
            </Button>
            <Button
              type="button"
              variant="outline"
              disabled={commands.pending}
              onClick={() => void commands.recover(true)}
            >
              Cancel if not applied
            </Button>
          </div>
        ) : (
          <p>
            Sign in with the account that started this submission to recover it.
          </p>
        ))
      )}
    </div>
  );
}
