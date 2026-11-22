import { useState, type FormEvent } from "react";
import { Failure } from "@/components/discussion/failure";
import { useSubmission } from "@/components/discussion/hooks";
import { useMutation } from "@/components/discussion/mutations";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import { Field, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { endpoint, useRequest, type Repository } from "@/lib/api";
import { IntegrationDeliveries } from "./integration-deliveries";

export interface Connection {
  id: string;
  name: string;
  github_repository: number;
  workflow: number;
  credential_version: string;
  url: string;
  dispatch?: {
    id: string;
    repository: string;
    installation: number;
    branch: string;
  } | null;
}

export interface Connections {
  available: boolean;
  dispatch_available?: boolean;
  items: Connection[];
}

interface Credentials {
  connection: Connection;
  secret?: string;
  fetch_token?: string;
}

type Confirmation = { connection: Connection; kind: "rotate" | "disconnect" };

export function IntegrationSettings({
  repo,
  csrf,
}: {
  repo: Repository;
  csrf: string;
}) {
  const path = endpoint(repo, "settings/integrations");
  const connections = useRequest<Connections>(path);
  const mutation = useMutation(csrf, repo);
  const receipt = useSubmission();
  const [credentials, setCredentials] = useState<Credentials>();
  const [confirmation, setConfirmation] = useState<Confirmation>();
  const [dispatchConnection, setDispatchConnection] = useState<string>();
  const [copied, setCopied] = useState("");
  const [copyError, setCopyError] = useState("");

  async function edit(change: object) {
    setCopied("");
    setCopyError("");
    const result = await mutation.run<Credentials | { disconnected: boolean }>(
      path,
      "POST",
      change,
    );
    if (!result) return false;
    setCredentials(
      "connection" in result && (result.secret || result.fetch_token)
        ? result
        : undefined,
    );
    setDispatchConnection(undefined);
    setConfirmation(undefined);
    connections.retry();
    return true;
  }

  async function confirm({ connection, kind }: Confirmation) {
    const action = {
      kind,
      id: connection.id,
      expected: connection.credential_version,
    };
    await edit(
      kind === "rotate" ? { ...action, credential: receipt(action) } : action,
    );
  }

  async function copy(label: string, value: string) {
    setCopyError("");
    try {
      await navigator.clipboard.writeText(value);
      setCopied(`${label} copied`);
    } catch {
      setCopyError(
        `Select and copy the ${label.toLowerCase()} from its field.`,
      );
    }
  }

  return (
    <section className="settings-content" aria-labelledby="integrations-title">
      <div className="settings-title">
        <div>
          <h2 id="integrations-title">Integrations</h2>
          <p>Connect GitHub Actions checks to this repository.</p>
        </div>
      </div>
      {credentials && (
        <section
          className="settings-section grid gap-4 p-4"
          aria-labelledby="webhook-credentials"
        >
          <h3 id="webhook-credentials" className="font-semibold">
            Configure {credentials.connection.name}
          </h3>
          {credentials.secret && (
            <p className="text-sm text-muted-foreground">
              Use this URL and secret in a private GitHub App with read-only
              Actions permissions. Subscribe to Workflow run and install the app
              on the source repository. Fork pull requests show the Git refspec
              for mirroring their current commit in the Checks tab.
            </p>
          )}
          {credentials.fetch_token && (
            <p className="text-sm text-muted-foreground">
              Save the queue checkout token as{" "}
              <code>CRAB_QUEUE_READ_TOKEN</code> in the connected GitHub
              repository.
            </p>
          )}
          {[
            {
              id: "ci-url",
              label: "Webhook URL",
              value: credentials.secret
                ? credentials.connection.url
                : undefined,
            },
            {
              id: "ci-secret",
              label: "Webhook secret",
              value: credentials.secret,
            },
            {
              id: "ci-fetch-token",
              label: "Queue checkout token",
              value: credentials.fetch_token,
            },
          ]
            .filter(
              (field): field is { id: string; label: string; value: string } =>
                !!field.value,
            )
            .map(({ id, label, value }) => (
              <div key={id} className="grid gap-1 text-sm font-medium">
                <label htmlFor={id}>{label}</label>
                <div className="flex gap-2">
                  <Input
                    id={id}
                    readOnly
                    value={value}
                    className="min-w-0 font-mono"
                  />
                  <Button
                    type="button"
                    size="sm"
                    variant="outline"
                    onClick={() => void copy(label, value)}
                  >
                    Copy {label.toLowerCase()}
                  </Button>
                </div>
              </div>
            ))}
          <p role="status" className="sr-only">
            {copied}
          </p>
          <Failure message={copyError} />
          <Button
            className="w-fit"
            size="sm"
            variant="outline"
            onClick={() => {
              setCredentials(undefined);
              setCopyError("");
            }}
          >
            Hide credentials
          </Button>
        </section>
      )}
      <Failure message={mutation.error} />
      <Result state={connections} timingLabel="CI connections">
        {({ available, dispatch_available, items }) => (
          <section
            className="settings-section"
            aria-labelledby="github-actions-title"
          >
            <header>
              <div>
                <h3 id="github-actions-title">GitHub Actions</h3>
                <p>
                  Each connection is scoped to a GitHub repository and workflow.
                </p>
              </div>
            </header>
            {!available ? (
              <p className="px-4 py-3 text-sm text-muted-foreground">
                GitHub Actions needs a shared authentication state key on the
                server.
              </p>
            ) : !repo.archived ? (
              <ConnectForm pending={mutation.pending} onConnect={edit} />
            ) : null}
            {items.length ? (
              <ul className="divide-y border-t">
                {items.map((connection) => (
                  <li key={connection.id} className="grid gap-3 p-4">
                    <div className="flex flex-wrap items-center justify-between gap-2">
                      <div className="min-w-0">
                        <strong className="break-words text-sm">
                          {connection.name}
                        </strong>
                        <p className="text-sm text-muted-foreground">
                          GitHub repository {connection.github_repository} ·
                          workflow {connection.workflow}
                        </p>
                        {connection.dispatch && (
                          <p className="text-sm text-muted-foreground">
                            Queue dispatch on {connection.dispatch.repository} ·{" "}
                            {connection.dispatch.branch}
                          </p>
                        )}
                      </div>
                      <div className="flex flex-wrap gap-2">
                        <IntegrationDeliveries
                          repo={repo}
                          connection={connection}
                        />
                        {(dispatch_available || connection.dispatch) &&
                          !repo.archived && (
                            <Button
                              size="sm"
                              variant="outline"
                              disabled={mutation.pending}
                              onClick={() => {
                                setConfirmation(undefined);
                                setDispatchConnection(connection.id);
                              }}
                            >
                              {connection.dispatch
                                ? "Edit queue dispatch"
                                : "Set up queue dispatch"}
                            </Button>
                          )}
                        <Button
                          size="sm"
                          variant="outline"
                          disabled={mutation.pending || !available}
                          aria-label={`Rotate secret for ${connection.name}`}
                          onClick={() =>
                            setConfirmation({ connection, kind: "rotate" })
                          }
                        >
                          Rotate secret
                        </Button>
                        <Button
                          size="sm"
                          variant="outline"
                          disabled={mutation.pending}
                          aria-label={`Disconnect ${connection.name}`}
                          onClick={() =>
                            setConfirmation({ connection, kind: "disconnect" })
                          }
                        >
                          Disconnect
                        </Button>
                      </div>
                    </div>
                    {dispatchConnection === connection.id && (
                      <DispatchForm
                        key={connection.id}
                        connection={connection}
                        available={!!dispatch_available}
                        pending={mutation.pending}
                        onSave={edit}
                        onCancel={() => setDispatchConnection(undefined)}
                      />
                    )}
                    {confirmation?.connection.id === connection.id && (
                      <div
                        className="grid gap-2 rounded-md border bg-muted/50 p-3"
                        role="region"
                        aria-label="Confirm workflow change"
                      >
                        <p className="text-sm">
                          {confirmation.kind === "rotate"
                            ? "Update the GitHub App with the new secret after rotating."
                            : "Branch rules that require this workflow will block merges until the rule is updated."}
                        </p>
                        <div className="flex flex-wrap gap-2">
                          <Button
                            size="sm"
                            variant="outline"
                            disabled={mutation.pending}
                            onClick={() => setConfirmation(undefined)}
                          >
                            Cancel
                          </Button>
                          <Button
                            size="sm"
                            variant={
                              confirmation.kind === "disconnect"
                                ? "destructive"
                                : "default"
                            }
                            disabled={mutation.pending}
                            onClick={() => void confirm(confirmation)}
                          >
                            {confirmation.kind === "rotate"
                              ? "Confirm rotation"
                              : "Confirm disconnect"}
                          </Button>
                        </div>
                      </div>
                    )}
                  </li>
                ))}
              </ul>
            ) : (
              <p className="border-t px-4 py-3 text-sm text-muted-foreground">
                No workflows connected.
              </p>
            )}
          </section>
        )}
      </Result>
    </section>
  );
}

function DispatchForm({
  connection,
  available,
  pending,
  onSave,
  onCancel,
}: {
  connection: Connection;
  available: boolean;
  pending: boolean;
  onSave: (change: object) => Promise<boolean>;
  onCancel: () => void;
}) {
  const [repository, setRepository] = useState(
    connection.dispatch?.repository ?? "",
  );
  const [branch, setBranch] = useState(connection.dispatch?.branch ?? "");
  const [installation, setInstallation] = useState(
    String(connection.dispatch?.installation ?? ""),
  );
  const submission = useSubmission();

  function edit(dispatch: NonNullable<Connection["dispatch"]> | null) {
    void onSave({
      kind: "dispatch",
      id: connection.id,
      expected: connection.credential_version,
      previous: connection.dispatch?.id ?? null,
      dispatch,
    });
  }

  return (
    <form
      className="grid gap-3 rounded-md border p-3"
      aria-label="Automatic queue dispatch"
      onSubmit={(event) => {
        event.preventDefault();
        const config = {
          repository: repository.trim(),
          branch: branch.trim(),
          installation: Number(installation),
        };
        edit({ ...config, id: submission(config) });
      }}
    >
      <p className="text-sm text-muted-foreground">
        Start this required workflow when a merge candidate reaches the front of
        the queue. Use a trusted workflow branch on the installed GitHub App
        repository.
      </p>
      <Field>
        <FieldLabel htmlFor="queue-dispatch-repository">
          GitHub owner and repository
        </FieldLabel>
        <Input
          id="queue-dispatch-repository"
          required
          value={repository}
          onChange={(event) => setRepository(event.target.value)}
          placeholder="owner/repository"
          disabled={pending || !available}
        />
      </Field>
      <Field>
        <FieldLabel htmlFor="queue-dispatch-installation">
          GitHub App installation ID
        </FieldLabel>
        <Input
          id="queue-dispatch-installation"
          required
          inputMode="numeric"
          pattern="[0-9]+"
          value={installation}
          onChange={(event) => setInstallation(event.target.value)}
          disabled={pending || !available}
        />
      </Field>
      <Field>
        <FieldLabel htmlFor="queue-dispatch-branch">
          GitHub workflow branch
        </FieldLabel>
        <Input
          id="queue-dispatch-branch"
          required
          value={branch}
          onChange={(event) => setBranch(event.target.value)}
          disabled={pending || !available}
        />
      </Field>
      <div className="flex flex-wrap gap-2">
        <Button
          type="submit"
          size="sm"
          disabled={
            !available ||
            pending ||
            !Number.isSafeInteger(Number(installation)) ||
            Number(installation) <= 0
          }
        >
          Save queue dispatch
        </Button>
        {connection.dispatch && (
          <Button
            type="button"
            size="sm"
            variant="outline"
            disabled={pending}
            onClick={() => edit(null)}
          >
            Disable queue dispatch
          </Button>
        )}
        <Button
          type="button"
          size="sm"
          variant="ghost"
          disabled={pending}
          onClick={onCancel}
        >
          Cancel
        </Button>
      </div>
    </form>
  );
}

function ConnectForm({
  pending,
  onConnect,
}: {
  pending: boolean;
  onConnect: (change: object) => Promise<boolean>;
}) {
  const submission = useSubmission();
  const [name, setName] = useState("");
  const [repository, setRepository] = useState("");
  const [workflow, setWorkflow] = useState("");
  const validId = (value: string) =>
    /^[1-9]\d*$/.test(value) && Number.isSafeInteger(Number(value));
  const valid = name.trim() !== "" && validId(repository) && validId(workflow);
  function submit(event: FormEvent) {
    event.preventDefault();
    if (!valid || pending) return;
    const input = {
      kind: "connect",
      name: name.trim(),
      github_repository: Number(repository),
      workflow: Number(workflow),
    };
    void onConnect({ ...input, id: submission(input) });
  }

  return (
    <form
      aria-label="Connect GitHub workflow"
      className="grid gap-3 p-4 sm:grid-cols-2"
      onSubmit={submit}
    >
      <Field className="sm:col-span-2">
        <FieldLabel htmlFor="ci-check">Check name</FieldLabel>
        <Input
          id="ci-check"
          value={name}
          onChange={(event) => setName(event.target.value)}
          required
          maxLength={100}
          disabled={pending}
        />
      </Field>
      <Field>
        <FieldLabel htmlFor="ci-repository">GitHub repository ID</FieldLabel>
        <Input
          id="ci-repository"
          inputMode="numeric"
          value={repository}
          onChange={(event) => setRepository(event.target.value)}
          required
          pattern="[1-9][0-9]*"
          maxLength={16}
          disabled={pending}
        />
      </Field>
      <Field>
        <FieldLabel htmlFor="ci-workflow">GitHub workflow ID</FieldLabel>
        <Input
          id="ci-workflow"
          inputMode="numeric"
          value={workflow}
          onChange={(event) => setWorkflow(event.target.value)}
          required
          pattern="[1-9][0-9]*"
          maxLength={16}
          disabled={pending}
        />
      </Field>
      <Button
        type="submit"
        size="sm"
        className="w-fit"
        disabled={!valid || pending}
      >
        Connect workflow
      </Button>
    </form>
  );
}
