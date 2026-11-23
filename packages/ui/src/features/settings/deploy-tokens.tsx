import { useState } from "react";
import { useMutation } from "@/components/discussion/mutations";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import { Field, FieldGroup, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import {
  NativeSelect,
  NativeSelectOption,
} from "@/components/ui/native-select";
import { endpoint, useRequest, type Repository } from "@/lib/api";

type DeployToken = {
  id: string;
  name: string;
  expires_at: number;
  creator_name: string;
};

type Edit =
  | { kind: "create"; name: string; days: number }
  | { kind: "revoke"; id: string };

const expires = new Intl.DateTimeFormat(undefined, {
  dateStyle: "medium",
  timeStyle: "short",
});

export function DeployTokens({
  repo,
  csrf,
}: {
  repo: Repository;
  csrf: string;
}) {
  const path = endpoint(repo, "settings/deploy-tokens");
  const state = useRequest<{ version: number; tokens: DeployToken[] }>(
    path,
    repo.id,
  );
  const mutation = useMutation(csrf, repo);
  const [name, setName] = useState("");
  const [days, setDays] = useState("7");
  const [secret, setSecret] = useState<string>();
  const [confirm, setConfirm] = useState<string>();
  const [message, setMessage] = useState<string>();

  async function change(expected_version: number, edit: Edit) {
    setMessage(undefined);
    const saved = await mutation.run<{ token: string | null }>(path, "POST", {
      expected_version,
      edit,
    });
    if (!saved) return;
    setSecret(saved.token ?? undefined);
    setConfirm(undefined);
    if (edit.kind === "create") setName("");
    else setMessage("Deploy token revoked.");
    state.retry();
  }

  return (
    <section className="settings-content" aria-labelledby="deploy-tokens-title">
      <div className="settings-title">
        <div>
          <h2 id="deploy-tokens-title">Deploy tokens</h2>
          <p>
            Give CI read-only access to this repository over Git and Git LFS.
          </p>
        </div>
      </div>
      {secret && (
        <section className="settings-section grid gap-3 p-4">
          <Field>
            <FieldLabel htmlFor="deploy-token-secret">Deploy token</FieldLabel>
            <Input
              id="deploy-token-secret"
              className="font-mono"
              autoComplete="off"
              readOnly
              value={secret}
            />
          </Field>
          <p className="text-sm">
            Save this token in your CI secret store and use it as the password
            for the Git username <code>crab</code>. It is shown only once.
          </p>
          <div className="flex flex-wrap gap-2">
            <Button
              size="sm"
              onClick={async () => {
                try {
                  await navigator.clipboard.writeText(secret);
                  setMessage("Deploy token copied.");
                } catch {
                  setMessage("Select the token field to copy it manually.");
                }
              }}
            >
              Copy deploy token
            </Button>
            <Button
              size="sm"
              variant="outline"
              onClick={() => {
                setSecret(undefined);
                setMessage(undefined);
              }}
            >
              Done
            </Button>
          </div>
        </section>
      )}
      {message && <p role="status">{message}</p>}
      {mutation.error && (
        <div className="notice error" role="alert">
          <p>
            {mutation.status === undefined
              ? "The response was lost. Reload to check whether the operation completed. If a new token appears, revoke it and create another."
              : mutation.error}
          </p>
          {(mutation.status === undefined || mutation.status === 409) && (
            <Button
              variant="outline"
              onClick={() => {
                mutation.reset();
                setConfirm(undefined);
                state.retry();
              }}
            >
              Reload deploy tokens
            </Button>
          )}
        </div>
      )}
      <Result state={state} timingLabel="Deploy tokens">
        {({ tokens, version }) => (
          <>
            {!repo.archived && (
              <form
                className="settings-section p-4"
                onSubmit={(event) => {
                  event.preventDefault();
                  void change(version, {
                    kind: "create",
                    name,
                    days: Number(days),
                  });
                }}
              >
                <FieldGroup>
                  <Field>
                    <FieldLabel htmlFor="deploy-token-name">Name</FieldLabel>
                    <Input
                      id="deploy-token-name"
                      value={name}
                      onChange={(event) => setName(event.target.value)}
                      required
                      maxLength={80}
                    />
                  </Field>
                  <Field>
                    <FieldLabel htmlFor="deploy-token-expiration">
                      Expiration
                    </FieldLabel>
                    <NativeSelect
                      id="deploy-token-expiration"
                      value={days}
                      onChange={(event) => setDays(event.target.value)}
                    >
                      <NativeSelectOption value="1">1 day</NativeSelectOption>
                      <NativeSelectOption value="7">7 days</NativeSelectOption>
                      <NativeSelectOption value="30">
                        30 days
                      </NativeSelectOption>
                    </NativeSelect>
                  </Field>
                  <Button
                    type="submit"
                    className="self-start"
                    disabled={
                      mutation.pending || !name.trim() || Boolean(secret)
                    }
                  >
                    Create deploy token
                  </Button>
                </FieldGroup>
              </form>
            )}
            {tokens.length ? (
              <ul className="settings-section divide-y">
                {tokens.map((token) => (
                  <li key={token.id} className="grid gap-3 p-4">
                    <div className="flex flex-wrap items-start justify-between gap-3">
                      <div className="min-w-0">
                        <strong className="wrap-break-word">
                          {token.name}
                        </strong>
                        <p className="text-sm text-muted-foreground">
                          Expires {expires.format(token.expires_at * 1_000)}
                          {" · "}Created by {token.creator_name}
                        </p>
                      </div>
                      <Button
                        size="sm"
                        variant="outline"
                        onClick={() => setConfirm(token.id)}
                        aria-label={`Revoke ${token.name}`}
                        disabled={mutation.pending}
                      >
                        Revoke
                      </Button>
                    </div>
                    {confirm === token.id && (
                      <div
                        className="grid gap-2"
                        role="group"
                        aria-label="Confirm revocation"
                      >
                        <p className="text-sm">
                          CI jobs using this token will lose access.
                        </p>
                        <div className="flex flex-wrap gap-2">
                          <Button
                            size="sm"
                            variant="outline"
                            disabled={mutation.pending}
                            onClick={() => setConfirm(undefined)}
                          >
                            Cancel
                          </Button>
                          <Button
                            size="sm"
                            variant="destructive"
                            disabled={mutation.pending}
                            onClick={() =>
                              void change(version, {
                                kind: "revoke",
                                id: token.id,
                              })
                            }
                          >
                            Confirm revoke
                          </Button>
                        </div>
                      </div>
                    )}
                  </li>
                ))}
              </ul>
            ) : (
              <p className="settings-section p-4">No active deploy tokens.</p>
            )}
          </>
        )}
      </Result>
    </section>
  );
}
