import {
  createContext,
  useContext,
  useEffect,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { endpoint, type Repository } from "@/lib/api";
import type { Command, Outcome } from "./model";
interface Intent {
  account: string;
  repo: Repository;
  body: { request_id: string; command: Command };
}
interface Commands {
  submit: (repo: Repository, command: Command) => Promise<Outcome | undefined>;
  locked: boolean;
  generation: number;
  error?: string;
  clearError: () => void;
  pending: boolean;
  canRecover: boolean;
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
  const [pending, setPending] = useState(false);
  useEffect(() => {
    if (!intent) return;
    const preserve = (event: BeforeUnloadEvent) => event.preventDefault();
    window.addEventListener("beforeunload", preserve);
    return () => window.removeEventListener("beforeunload", preserve);
  }, [intent]);
  async function send(captured: Intent, cancel: boolean) {
    if (busy.current || account !== captured.account) return;
    busy.current = true;
    setPending(true);
    setError(undefined);
    try {
      // The captured repository, account, UUID, and payload survive route changes.
      // An unproven response never releases the intent or permits a replacement write.
      const response = await fetch(
        endpoint(captured.repo, `projects/${cancel ? "cancel" : "commands"}`),
        {
          method: "POST",
          headers: {
            "Content-Type": "application/json",
            "X-CSRF-Token": csrf,
            "X-Crab-Repository-Id": captured.repo.id,
          },
          body: JSON.stringify(captured.body),
          signal: AbortSignal.timeout(35000),
        },
      );
      if (response.status === 401)
        window.dispatchEvent(new Event("crab-session-expired"));
      const result: unknown = await response.json().catch(() => null);
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
          value.outcome !== "cancelled" &&
          value.outcome !== "rejected")
      )
        throw new Error("The server acknowledgement was invalid.");
      if (value.outcome === "applied") {
        const command = captured.body.command;
        const expected =
          command.kind === "parent" || command.kind === "sub_issue"
            ? null
            : command.kind === "create" || command.kind === "copy"
              ? command.id
              : command.project;
        if (
          value.project !== expected ||
          !Number.isSafeInteger(value.version) ||
          value.version < 0
        )
          throw new Error(
            "The server acknowledgement did not match this submission.",
          );
        setGeneration((previous) => previous + 1);
      }
      current.current = null;
      setIntent(null);
      if (value.outcome === "rejected") setError(value.reason);
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
  function submit(repo: Repository, command: Command) {
    if (!account || current.current || busy.current)
      return Promise.resolve(undefined);
    const captured: Intent = {
      account,
      repo: { ...repo },
      body: { request_id: crypto.randomUUID(), command },
    };
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
        submit,
        locked: !!intent,
        generation,
        error,
        pending,
        clearError: () => setError(undefined),
        canRecover: !!intent && intent.account === account,
        recover: (cancel) =>
          current.current
            ? send(current.current, cancel)
            : Promise.resolve(undefined),
      }}
    >
      {(intent || error) && (
        <Alert className="project-feedback" aria-live="polite">
          <AlertTitle>
            {intent ? "Project submission" : "Changes were not saved"}
          </AlertTitle>
          <AlertDescription>
            {intent?.account !== account && intent ? (
              <p>
                Sign in with the account that started this submission to recover
                it.
              </p>
            ) : (
              <>
                <p>{error ?? "Saving your changes…"}</p>
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
                    onClick={() => setError(undefined)}
                  >
                    Dismiss
                  </Button>
                )}
              </>
            )}
          </AlertDescription>
        </Alert>
      )}
      {children}
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
      {commands.locked &&
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
        ))}
    </div>
  );
}
