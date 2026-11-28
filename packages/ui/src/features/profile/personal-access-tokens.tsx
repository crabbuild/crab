import { KeyIcon } from "@primer/octicons-react";
import { useState } from "react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { FieldSelect } from "@/components/ui/field-select";
import { Input } from "@/components/ui/input";
import { useRequest } from "@/lib/api";
import type { Session } from "@/lib/contracts";

interface TokenSummary {
  id: string;
  name: string;
  scopes: string[];
  created_at: number;
  expires_at: number;
  expired: boolean;
}

interface CreatedToken extends TokenSummary {
  token: string;
}

// GitHub CLI asks for repo, read:org, and gist; Crab grants nothing for gist
// or workflow but accepts them so gh's scope check passes.
const SCOPES = [
  {
    scope: "repo",
    label: "repo",
    description:
      "Repositories you can access, up to your own role. Without it, public repositories are read-only.",
  },
  {
    scope: "read:org",
    label: "read:org",
    description: "Organization and team memberships.",
  },
  {
    scope: "delete_repo",
    label: "delete_repo",
    description: "Delete repositories you administer.",
  },
  {
    scope: "gist",
    label: "gist",
    description: "Accepted for GitHub CLI; Crab has no gists.",
  },
  {
    scope: "workflow",
    label: "workflow",
    description: "Accepted for GitHub CLI; grants no extra access.",
  },
] as const;

const LIFETIMES = [
  { value: "7", label: "7 days" },
  { value: "30", label: "30 days" },
  { value: "60", label: "60 days" },
  { value: "90", label: "90 days" },
  { value: "366", label: "1 year" },
];

function day(seconds: number) {
  return new Date(seconds * 1000).toLocaleDateString(undefined, {
    year: "numeric",
    month: "short",
    day: "numeric",
  });
}

export function PersonalAccessTokensSettingsPanel({
  session,
}: {
  session: Session;
}) {
  const tokens = useRequest<TokenSummary[]>(
    session.user ? "/api/personal-access-tokens" : null,
  );
  const [name, setName] = useState("GitHub CLI");
  const [lifetime, setLifetime] = useState("30");
  const [scopes, setScopes] = useState<string[]>(["repo", "read:org", "gist"]);
  const [created, setCreated] = useState<CreatedToken>();
  const [revealed, setRevealed] = useState(false);
  const [pending, setPending] = useState(false);
  const [message, setMessage] = useState<string>();
  const host = window.location.host;

  async function send(method: "POST" | "DELETE", path: string, body?: object) {
    const response = await fetch(path, {
      method,
      headers: {
        "X-CSRF-Token": session.csrf ?? "",
        "Content-Type": "application/json",
      },
      body: body ? JSON.stringify(body) : undefined,
    });
    if (!response.ok) {
      throw new Error(
        method === "POST"
          ? "Could not create the token. Check the name and scopes, then try again."
          : "Could not revoke the token. Reload and try again.",
      );
    }
    return response;
  }

  async function create() {
    setPending(true);
    setMessage(undefined);
    setCreated(undefined);
    setRevealed(false);
    try {
      const response = await send("POST", "/api/personal-access-tokens", {
        name: name.trim(),
        scopes,
        expires_in_days: Number(lifetime),
      });
      setCreated((await response.json()) as CreatedToken);
      tokens.retry();
    } catch (error) {
      setMessage(error instanceof Error ? error.message : String(error));
    } finally {
      setPending(false);
    }
  }

  async function revoke(token: TokenSummary) {
    setPending(true);
    setMessage(undefined);
    try {
      await send("DELETE", `/api/personal-access-tokens/${token.id}`);
      if (created?.id === token.id) setCreated(undefined);
      setMessage(`Revoked “${token.name}”.`);
      tokens.retry();
    } catch (error) {
      setMessage(error instanceof Error ? error.message : String(error));
    } finally {
      setPending(false);
    }
  }

  return (
    <section className="grid min-w-0 gap-6">
      <div className="border-b pb-4">
        <h1 className="mb-3 text-2xl font-semibold tracking-tight">
          Personal access tokens
        </h1>
        <p className="m-0 text-sm text-muted-foreground">
          Use a token with the GitHub CLI and other GitHub-compatible tools.
          Tokens act as your account for the GitHub API and HTTPS Git, never for
          this browser interface.
        </p>
      </div>

      {!session.user ? (
        <p className="m-0 rounded-lg border border-border bg-card p-4 text-sm text-muted-foreground">
          Local sessions do not issue personal access tokens.
        </p>
      ) : (
        <>
          <form
            className="grid gap-4 rounded-lg border border-border bg-card p-4"
            onSubmit={(event) => {
              event.preventDefault();
              void create();
            }}
          >
            <div className="flex items-center gap-2">
              <KeyIcon size={16} className="text-muted-foreground" />
              <h2 className="m-0 text-base font-semibold">New token</h2>
            </div>
            <div className="grid gap-3 sm:grid-cols-2">
              <div className="grid gap-1.5 text-sm font-medium">
                <label htmlFor="personal-token-name">Note</label>
                <Input
                  id="personal-token-name"
                  value={name}
                  maxLength={100}
                  required
                  onChange={(event) => setName(event.target.value)}
                />
              </div>
              <div className="grid gap-1.5 text-sm font-medium">
                <label htmlFor="personal-token-lifetime">Expiration</label>
                <FieldSelect
                  id="personal-token-lifetime"
                  label="Expiration"
                  value={lifetime}
                  onValueChange={setLifetime}
                  options={LIFETIMES}
                />
              </div>
            </div>
            <fieldset className="m-0 grid gap-2 border-0 p-0">
              <legend className="mb-1 text-sm font-medium">Scopes</legend>
              {SCOPES.map((option) => (
                <label
                  key={option.scope}
                  className="flex items-start gap-2 text-sm"
                >
                  <Checkbox
                    className="mt-0.5"
                    checked={scopes.includes(option.scope)}
                    onCheckedChange={(checked) =>
                      setScopes((current) =>
                        checked === true
                          ? [...current, option.scope]
                          : current.filter((scope) => scope !== option.scope),
                      )
                    }
                  />
                  <span>
                    <code>{option.label}</code>
                    <span className="block text-xs text-muted-foreground">
                      {option.description}
                    </span>
                  </span>
                </label>
              ))}
            </fieldset>
            <div>
              <Button type="submit" disabled={pending || !name.trim()}>
                Generate token
              </Button>
            </div>
          </form>

          {created && (
            <div className="grid gap-3 rounded-lg border border-border bg-muted/40 p-4">
              <p className="m-0 text-sm font-medium">
                Copy “{created.name}” now. It will not be shown again.
              </p>
              <div className="flex flex-wrap gap-2">
                <Input
                  aria-label="Personal access token"
                  type={revealed ? "text" : "password"}
                  readOnly
                  value={created.token}
                  className="min-w-0 flex-1 font-mono text-xs"
                />
                <Button
                  type="button"
                  variant="outline"
                  size="sm"
                  onClick={() => setRevealed((value) => !value)}
                >
                  {revealed ? "Hide" : "Reveal"}
                </Button>
                <Button
                  type="button"
                  variant="outline"
                  size="sm"
                  onClick={() =>
                    void navigator.clipboard?.writeText(created.token)
                  }
                >
                  Copy
                </Button>
              </div>
              <p className="m-0 text-xs text-muted-foreground">
                Sign in with the GitHub CLI, then let it answer Git credential
                prompts for this host:
              </p>
              <code className="block whitespace-pre-wrap rounded-md border border-border bg-background p-2 text-xs">
                {`gh auth login --hostname ${host} --with-token < token.txt\ngh auth setup-git --hostname ${host}`}
              </code>
            </div>
          )}

          <div className="grid gap-2">
            <h2 className="m-0 text-base font-semibold">Your tokens</h2>
            {tokens.loading ? (
              <p className="m-0 text-sm text-muted-foreground">Loading…</p>
            ) : tokens.error ? (
              <p className="m-0 text-sm text-destructive">
                Tokens could not be loaded.
              </p>
            ) : !tokens.data?.length ? (
              <p className="m-0 text-sm text-muted-foreground">
                You have no personal access tokens.
              </p>
            ) : (
              <ul className="m-0 grid list-none divide-y divide-border rounded-lg border border-border p-0">
                {tokens.data.map((token) => (
                  <li
                    key={token.id}
                    className="flex flex-wrap items-center justify-between gap-3 p-3"
                  >
                    <div className="grid gap-1">
                      <span className="text-sm font-medium">
                        {token.name}{" "}
                        {token.expired && (
                          <Badge variant="outline">Expired</Badge>
                        )}
                      </span>
                      <span className="text-xs text-muted-foreground">
                        {token.scopes.length
                          ? token.scopes.join(", ")
                          : "No scopes"}{" "}
                        · Created {day(token.created_at)} ·{" "}
                        {token.expired ? "Expired" : "Expires"}{" "}
                        {day(token.expires_at)}
                      </span>
                    </div>
                    <Button
                      type="button"
                      variant="outline"
                      size="sm"
                      disabled={pending}
                      onClick={() => void revoke(token)}
                    >
                      Revoke
                    </Button>
                  </li>
                ))}
              </ul>
            )}
          </div>

          {message && (
            <p role="status" className="m-0 text-sm text-muted-foreground">
              {message}
            </p>
          )}
        </>
      )}
    </section>
  );
}
