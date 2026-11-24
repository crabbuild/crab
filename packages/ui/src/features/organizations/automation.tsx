import { useState } from "react";
import { useMutation } from "@/components/discussion/mutations";
import { Result } from "@/components/request/result";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Field, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import {
  NativeSelect,
  NativeSelectOption,
} from "@/components/ui/native-select";
import { useRequest } from "@/lib/api";
import type {
  AutomationAccess,
  AutomationGrant,
  AutomationIdentity,
  AutomationSnapshot,
} from "@/lib/contracts";
import { MutationError } from "./feedback";

type GrantInput = Pick<AutomationGrant, "repository" | "access">;
type Edit =
  | { kind: "create"; name: string; grants: GrantInput[] }
  | { kind: "configure"; identity: string; name: string; grants: GrantInput[] }
  | { kind: "enabled"; identity: string; enabled: boolean }
  | { kind: "delete"; identity: string }
  | { kind: "issue"; identity: string; hours: number }
  | { kind: "revoke"; identity: string; token: string };

const expiration = new Intl.DateTimeFormat(undefined, {
  dateStyle: "medium",
  timeStyle: "short",
});

export function OrganizationAutomation({
  organization,
  csrf,
}: {
  organization: string;
  csrf: string;
}) {
  const endpoint = `/api/organizations/${organization}/automation`;
  const state = useRequest<AutomationSnapshot>(endpoint);
  const mutation = useMutation(csrf, null);
  const [editor, setEditor] = useState<string>();
  const [secret, setSecret] = useState<string>();
  const [notice, setNotice] = useState("");

  async function change(version: number, edit: Edit) {
    setNotice("");
    const response = await mutation.run<{
      identity: string;
      token: string | null;
    }>(endpoint, "POST", { expected_version: version, edit });
    if (!response) return;
    setSecret(response.token ?? undefined);
    if (edit.kind === "create" || edit.kind === "configure")
      setEditor(undefined);
    setNotice(
      edit.kind === "issue"
        ? "Automation token issued."
        : edit.kind === "revoke"
          ? "Automation token revoked."
          : edit.kind === "delete"
            ? "Automation identity deleted."
            : edit.kind === "enabled"
              ? edit.enabled
                ? "Automation enabled."
                : "Automation disabled and tokens revoked."
              : "Automation access saved.",
    );
    state.retry();
  }

  return (
    <section className="grid gap-4" aria-labelledby="automation-title">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <h2 id="automation-title" className="text-lg font-semibold">
            Automation
          </h2>
          <p className="text-sm text-muted-foreground">
            Manage Git and Git LFS credentials for CI and other services.
          </p>
        </div>
        <Button
          disabled={mutation.pending || !state.data || !!editor}
          onClick={() => setEditor("new")}
        >
          New automation identity
        </Button>
      </div>
      {secret && (
        <Alert>
          <AlertDescription className="grid gap-2">
            <Field>
              <FieldLabel htmlFor="automation-secret">
                Automation token
              </FieldLabel>
              <Input
                id="automation-secret"
                readOnly
                autoComplete="off"
                value={secret}
                className="font-mono"
              />
            </Field>
            <p>
              Save this token in your CI secret store. Use it as the Git
              password for username <code>crab</code>.
            </p>
            <div className="flex gap-2">
              <Button
                size="sm"
                onClick={async () => {
                  try {
                    await navigator.clipboard.writeText(secret);
                    setNotice("Automation token copied.");
                  } catch {
                    setNotice("Select the token field to copy it.");
                  }
                }}
              >
                Copy token
              </Button>
              <Button
                size="sm"
                variant="outline"
                onClick={() => setSecret(undefined)}
              >
                Done
              </Button>
            </div>
          </AlertDescription>
        </Alert>
      )}
      {notice && (
        <p role="status" className="text-sm">
          {notice}
        </p>
      )}
      <MutationError error={mutation.error} />
      <Result state={state} timingLabel="Organization automation">
        {({ version, identities }) => (
          <>
            {editor === "new" && (
              <GrantEditor
                organization={organization}
                pending={mutation.pending}
                onCancel={() => setEditor(undefined)}
                onSave={(name, grants) =>
                  void change(version, { kind: "create", name, grants })
                }
              />
            )}
            {identities.length === 0 && (
              <p className="text-sm text-muted-foreground">
                No automation identities.
              </p>
            )}
            <div className="grid gap-4">
              {identities.map((identity) => (
                <IdentityCard
                  key={identity.id}
                  identity={identity}
                  pending={mutation.pending}
                  editing={editor === identity.id}
                  onEdit={() => setEditor(identity.id)}
                  onChange={(edit) => void change(version, edit)}
                >
                  <GrantEditor
                    organization={organization}
                    identity={identity}
                    pending={mutation.pending}
                    onCancel={() => setEditor(undefined)}
                    onSave={(name, grants) =>
                      void change(version, {
                        kind: "configure",
                        identity: identity.id,
                        name,
                        grants,
                      })
                    }
                  />
                </IdentityCard>
              ))}
            </div>
          </>
        )}
      </Result>
    </section>
  );
}

function IdentityCard({
  identity,
  pending,
  editing,
  onEdit,
  onChange,
  children,
}: {
  identity: AutomationIdentity;
  pending: boolean;
  editing: boolean;
  onEdit: () => void;
  onChange: (edit: Edit) => void;
  children: React.ReactNode;
}) {
  const [hours, setHours] = useState("24");
  const [confirm, setConfirm] = useState<Edit>();
  return (
    <article className="grid gap-3 rounded-lg border p-4">
      <header className="flex flex-wrap items-center justify-between gap-2">
        <div className="flex flex-wrap items-center gap-2">
          <h3 className="wrap-anywhere font-semibold">{identity.name}</h3>
          <Badge variant={identity.enabled ? "secondary" : "outline"}>
            {identity.enabled ? "Enabled" : "Disabled"}
          </Badge>
        </div>
        <div className="flex flex-wrap gap-2">
          <Button
            size="sm"
            variant="outline"
            disabled={pending || editing}
            onClick={onEdit}
          >
            Edit access
          </Button>
          {identity.enabled ? (
            <Button
              size="sm"
              variant="outline"
              disabled={pending}
              onClick={() =>
                setConfirm({
                  kind: "enabled",
                  identity: identity.id,
                  enabled: false,
                })
              }
            >
              Disable
            </Button>
          ) : (
            <>
              <Button
                size="sm"
                variant="outline"
                disabled={pending}
                onClick={() =>
                  onChange({
                    kind: "enabled",
                    identity: identity.id,
                    enabled: true,
                  })
                }
              >
                Enable
              </Button>
              <Button
                size="sm"
                variant="outline"
                disabled={pending}
                onClick={() =>
                  setConfirm({ kind: "delete", identity: identity.id })
                }
              >
                Delete identity
              </Button>
            </>
          )}
        </div>
      </header>
      {editing ? (
        children
      ) : identity.grants.length ? (
        <ul className="grid gap-1 text-sm">
          {identity.grants.map((grant) => (
            <li key={grant.repository} className="wrap-anywhere">
              {grant.name ?? grant.repository} ·{" "}
              {grant.access === "write" ? "Write" : "Read"}
              {!grant.current && " · Inactive grant"}
            </li>
          ))}
        </ul>
      ) : (
        <p className="text-sm text-muted-foreground">No repository grants.</p>
      )}
      {identity.enabled && (
        <div className="flex flex-wrap items-end gap-2">
          <Field className="w-auto">
            <FieldLabel htmlFor={`automation-expiry-${identity.id}`}>
              Token lifetime
            </FieldLabel>
            <NativeSelect
              id={`automation-expiry-${identity.id}`}
              value={hours}
              onChange={(e) => setHours(e.target.value)}
            >
              <NativeSelectOption value="1">1 hour</NativeSelectOption>
              <NativeSelectOption value="24">1 day</NativeSelectOption>
              <NativeSelectOption value="168">7 days</NativeSelectOption>
              <NativeSelectOption value="720">30 days</NativeSelectOption>
            </NativeSelect>
          </Field>
          <Button
            disabled={
              pending ||
              identity.tokens.length >= 4 ||
              !identity.grants.some((grant) => grant.current)
            }
            onClick={() =>
              onChange({
                kind: "issue",
                identity: identity.id,
                hours: Number(hours),
              })
            }
          >
            Issue token
          </Button>
        </div>
      )}
      {identity.tokens.length > 0 && (
        <ul
          aria-label="Active automation tokens"
          className="divide-y rounded-lg border"
        >
          {identity.tokens.map((token) => (
            <li
              key={token.id}
              className="flex flex-wrap items-center justify-between gap-2 p-2 text-sm"
            >
              <span className="wrap-anywhere">
                <code>{token.id}</code> · Expires{" "}
                {expiration.format(new Date(token.expires_at * 1000))}
              </span>
              <Button
                size="sm"
                variant="ghost"
                disabled={pending}
                onClick={() =>
                  setConfirm({
                    kind: "revoke",
                    identity: identity.id,
                    token: token.id,
                  })
                }
              >
                Revoke token
              </Button>
            </li>
          ))}
        </ul>
      )}
      {confirm && (
        <Alert>
          <AlertDescription>
            <p>
              {confirm.kind === "enabled"
                ? "Disable this identity and revoke all its tokens?"
                : confirm.kind === "delete"
                  ? "Delete this disabled identity?"
                  : "Revoke this token?"}
            </p>
            <div className="mt-2 flex gap-2">
              <Button
                variant="destructive"
                size="sm"
                disabled={pending}
                onClick={() => {
                  setConfirm(undefined);
                  onChange(confirm);
                }}
              >
                Confirm
              </Button>
              <Button
                variant="outline"
                size="sm"
                onClick={() => setConfirm(undefined)}
              >
                Cancel
              </Button>
            </div>
          </AlertDescription>
        </Alert>
      )}
    </article>
  );
}

type RepoPage = {
  repositories: { id: string; name: string }[];
  next: string | null;
};

function GrantEditor({
  organization,
  identity,
  pending,
  onCancel,
  onSave,
}: {
  organization: string;
  identity?: AutomationIdentity;
  pending: boolean;
  onCancel: () => void;
  onSave: (name: string, grants: GrantInput[]) => void;
}) {
  const [name, setName] = useState(identity?.name ?? "");
  const [grants, setGrants] = useState(
    identity?.grants.filter((grant) => grant.current) ?? [],
  );
  const [term, setTerm] = useState("");
  const [search, setSearch] = useState("");
  const [after, setAfter] = useState<string>();
  const query = new URLSearchParams({ organization, q: search, limit: "8" });
  if (after) query.set("after", after);
  const repositories = useRequest<RepoPage>(`/api/repos?${query}`);
  return (
    <form
      className="grid gap-3 rounded-lg border p-3"
      onSubmit={(event) => {
        event.preventDefault();
        onSave(
          name.trim(),
          grants.map(({ repository, access }) => ({ repository, access })),
        );
      }}
    >
      <Field>
        <FieldLabel htmlFor="automation-name">Identity name</FieldLabel>
        <Input
          id="automation-name"
          value={name}
          maxLength={80}
          required
          onChange={(event) => setName(event.target.value)}
        />
      </Field>
      {grants.length > 0 && (
        <ul className="grid gap-2">
          {grants.map((grant) => (
            <li
              className="flex flex-wrap items-end gap-2"
              key={grant.repository}
            >
              <Field className="w-auto">
                <FieldLabel htmlFor={`automation-access-${grant.repository}`}>
                  Access to {grant.name}
                </FieldLabel>
                <NativeSelect
                  id={`automation-access-${grant.repository}`}
                  value={grant.access}
                  onChange={(event) => {
                    const access = event.target.value as AutomationAccess;
                    setGrants((items) =>
                      items.map((item) =>
                        item.repository === grant.repository
                          ? { ...item, access }
                          : item,
                      ),
                    );
                  }}
                >
                  <NativeSelectOption value="read">Read</NativeSelectOption>
                  <NativeSelectOption value="write">Write</NativeSelectOption>
                </NativeSelect>
              </Field>
              <Button
                size="sm"
                variant="ghost"
                type="button"
                onClick={() =>
                  setGrants((items) =>
                    items.filter(
                      (item) => item.repository !== grant.repository,
                    ),
                  )
                }
              >
                Remove grant
              </Button>
            </li>
          ))}
        </ul>
      )}
      <div className="flex flex-wrap items-end gap-2">
        <Field className="min-w-48 flex-1">
          <FieldLabel htmlFor="automation-repo-search">
            Find repositories
          </FieldLabel>
          <Input
            id="automation-repo-search"
            type="search"
            value={term}
            maxLength={200}
            onChange={(event) => setTerm(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter") {
                event.preventDefault();
                setAfter(undefined);
                setSearch(term.trim());
              }
            }}
          />
        </Field>
        <Button
          type="button"
          variant="outline"
          onClick={() => {
            setAfter(undefined);
            setSearch(term.trim());
          }}
        >
          Search
        </Button>
      </div>
      <Result state={repositories} timingLabel="Automation repository choices">
        {({ repositories, next }) => (
          <div className="flex flex-wrap gap-2">
            {repositories.map((repo) => (
              <Button
                key={repo.id}
                type="button"
                size="sm"
                variant="outline"
                disabled={
                  grants.length >= 50 ||
                  grants.some((grant) => grant.repository === repo.id)
                }
                onClick={() =>
                  setGrants((items) => [
                    ...items,
                    {
                      repository: repo.id,
                      name: repo.name,
                      access: "read",
                      current: true,
                    },
                  ])
                }
              >
                Add {repo.name}
              </Button>
            ))}
            {next && (
              <Button
                type="button"
                size="sm"
                variant="ghost"
                onClick={() => setAfter(next)}
              >
                More repositories
              </Button>
            )}
            {after && (
              <Button
                type="button"
                size="sm"
                variant="ghost"
                onClick={() => setAfter(undefined)}
              >
                First page
              </Button>
            )}
            {!repositories.length && (
              <p className="text-sm text-muted-foreground">
                No matching repositories.
              </p>
            )}
          </div>
        )}
      </Result>
      <div className="flex gap-2">
        <Button type="submit" disabled={pending || !name.trim()}>
          {identity ? "Save access" : "Create identity"}
        </Button>
        <Button type="button" variant="outline" onClick={onCancel}>
          Cancel
        </Button>
      </div>
    </form>
  );
}
