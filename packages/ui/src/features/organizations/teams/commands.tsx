import { useEffect, useRef, useState } from "react";
import { useMutation } from "@/components/discussion/mutations";
import { Link } from "@/components/navigation/link";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import {
  teamHref,
  type Acknowledgement,
  type Command,
  type Submission,
} from "./model";

interface Intent {
  account: string;
  organization: string;
  actor: string;
  body: Submission;
  label: string;
}
interface Resolution {
  intent: Intent;
  acknowledgement: Acknowledgement;
}
export function useTeamCommands(csrf: string, account: string | null) {
  const mutation = useMutation(csrf, null);
  const current = useRef<Intent | null>(null);
  const busy = useRef(false);
  const [intent, setIntent] = useState<Intent | null>(null);
  const [resolved, setResolved] = useState<Resolution>();
  const [failure, setFailure] = useState<Intent>();
  const [invalid, setInvalid] = useState(false);
  const [generation, setGeneration] = useState(0);
  // Only a canonical rejection proves that the command made no change. An
  // access denial, conflict of submission IDs, or missing response retains it.
  useEffect(() => {
    if (!mutation.pending && mutation.rejected && current.current) {
      setFailure(current.current);
      current.current = null;
      setIntent(null);
      setGeneration((value) => value + 1);
    }
  }, [mutation.pending, mutation.rejected]);
  useEffect(() => {
    if (!intent) return;
    const preserve = (event: BeforeUnloadEvent) => event.preventDefault();
    window.addEventListener("beforeunload", preserve);
    return () => window.removeEventListener("beforeunload", preserve);
  }, [intent]);
  async function send(captured: Intent, cancel: boolean) {
    if (busy.current || captured.account !== account) return;
    busy.current = true;
    setInvalid(false);
    try {
      const value = await mutation.run<Acknowledgement>(
        `/api/organizations/${captured.organization}/teams/${cancel ? "cancel" : "commands"}`,
        "POST",
        captured.body,
      );
      if (!value) return;
      const { command, request_id } = captured.body;
      const target =
        command.kind === "create"
          ? request_id
          : command.kind === "policy"
            ? captured.organization
            : command.team;
      if (
        value.id !== target ||
        !["applied", "cancelled"].includes(value.outcome)
      ) {
        setInvalid(true);
        return;
      }
      current.current = null;
      setIntent(null);
      setFailure(undefined);
      setResolved({ intent: captured, acknowledgement: value });
      setGeneration((previous) => previous + 1);
      return value;
    } finally {
      busy.current = false;
    }
  }
  function submit(
    organization: string,
    actor: string,
    command: Command,
    label: string,
  ) {
    if (!account || current.current || busy.current)
      return Promise.resolve(undefined);
    const captured: Intent = {
      account,
      organization,
      actor,
      body: { request_id: crypto.randomUUID(), command },
      label,
    };
    current.current = captured;
    setIntent(captured);
    setResolved(undefined);
    setFailure(undefined);
    return send(captured, false);
  }
  return {
    submit,
    dismiss: () => {
      setFailure(undefined);
      setResolved(undefined);
      mutation.reset();
    },
    account,
    intent,
    resolved,
    failure,
    invalid,
    generation,
    pending: mutation.pending,
    locked: !!intent,
    error: mutation.error,
    conflict: mutation.conflict,
    recover: (cancel: boolean) =>
      current.current
        ? send(current.current, cancel)
        : Promise.resolve(undefined),
  };
}
export type Commands = ReturnType<typeof useTeamCommands>;
export function TeamFeedback({ commands }: { commands: Commands }) {
  const { intent } = commands;
  const resolved =
    commands.resolved?.intent.account === commands.account
      ? commands.resolved
      : undefined;
  if (intent && intent.account !== commands.account)
    return (
      <Alert className="mb-5">
        <AlertDescription>
          Sign in with the account that started the team submission to resolve
          it.
        </AlertDescription>
      </Alert>
    );
  if (intent)
    return (
      <Alert className="mb-5">
        <AlertTitle>
          {commands.pending
            ? `${intent.label}…`
            : "Team submission needs confirmation"}
        </AlertTitle>
        <AlertDescription>
          {!commands.pending && (
            <>
              <p>
                {commands.invalid
                  ? "The server returned an invalid acknowledgement."
                  : commands.error}
              </p>
              <p>
                Retry the original submission, or cancel it. Cancellation
                confirms a completed change or prevents an unapplied change from
                running later.
              </p>
              <OriginalIntent intent={intent} />
              <div className="flex flex-wrap gap-2">
                <Button
                  variant="outline"
                  onClick={() => void commands.recover(false)}
                >
                  Retry original submission
                </Button>
                <Button
                  variant="outline"
                  onClick={() => void commands.recover(true)}
                >
                  Cancel or recover submission
                </Button>
              </div>
            </>
          )}
        </AlertDescription>
      </Alert>
    );
  if (
    commands.error &&
    commands.failure &&
    commands.failure.account === commands.account
  )
    return (
      <Alert variant="destructive" className="mb-5">
        <AlertTitle>Team change was not saved</AlertTitle>
        <AlertDescription>
          {commands.error}
          <OriginalIntent intent={commands.failure} />
          <Button variant="ghost" onClick={commands.dismiss}>
            Dismiss team notification
          </Button>
        </AlertDescription>
      </Alert>
    );
  if (!resolved) return null;
  const cancelled = resolved.acknowledgement.outcome === "cancelled";
  return (
    <Alert className="mb-5" role="status" aria-label="Team submission result">
      <AlertDescription>
        {cancelled
          ? "Submission cancelled. A late request cannot apply this change."
          : `${resolved.intent.label} completed.`}
        {!cancelled && resolved.intent.body.command.kind === "create" && (
          <>
            {" "}
            <Link
              href={teamHref(
                resolved.intent.organization,
                resolved.acknowledgement.id,
              )}
            >
              Open team
            </Link>
          </>
        )}
        <Button variant="ghost" onClick={commands.dismiss}>
          Dismiss team notification
        </Button>
      </AlertDescription>
    </Alert>
  );
}

function OriginalIntent({ intent }: { intent: Intent }) {
  return (
    <details className="my-2">
      <summary>Original submission</summary>
      <p className="break-all">
        Account: {intent.actor} · Organization: {intent.organization}
      </p>
      <pre className="max-h-56 overflow-auto whitespace-pre-wrap break-all text-xs">
        {JSON.stringify(intent.body, null, 2)}
      </pre>
    </details>
  );
}
