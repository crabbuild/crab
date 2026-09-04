import { useState } from "react";
import { Button } from "@primer/react";
import type { Repository, Session } from "./api";

interface GitToken {
  username: string;
  token: string;
  expires_in: number;
}

export function GitAccess({ session }: { session: Session }) {
  const [token, setToken] = useState<GitToken>();
  const [pending, setPending] = useState(false);
  const [message, setMessage] = useState<string>();
  async function change(method: "POST" | "DELETE") {
    setPending(true);
    setMessage(undefined);
    try {
      const response = await fetch("/api/git-token", {
        method,
        headers: { "X-CSRF-Token": session.csrf ?? "" },
      });
      if (!response.ok)
        throw new Error("Could not update Git access. Reload and try again.");
      setToken(
        method === "POST" ? ((await response.json()) as GitToken) : undefined,
      );
      if (method === "DELETE")
        setMessage("Tokens from this sign-in have been revoked.");
    } catch (error) {
      setMessage(
        error instanceof Error ? error.message : "Could not update Git access",
      );
    } finally {
      setPending(false);
    }
  }
  return (
    <details className="git-access">
      <summary>Git access</summary>
      <div className="panel git-popover">
        <h2>Git access token</h2>
        <p>
          Use <code>crab</code> as your Git username and this token as the
          password. It can read your repositories and expires when this sign-in
          expires or you sign out.
        </p>
        <p>Save it in your Git credential manager. It is shown only here.</p>
        {token && (
          <>
            <label htmlFor="git-token">
              Token · expires in about {Math.ceil(token.expires_in / 60)}{" "}
              minutes
            </label>
            <input
              id="git-token"
              readOnly
              type="password"
              value={token.token}
              onFocus={(event) => event.currentTarget.select()}
              autoComplete="off"
            />
            <Button
              onClick={async () => {
                try {
                  await navigator.clipboard.writeText(token.token);
                  setMessage("Token copied.");
                } catch {
                  setMessage(
                    "Clipboard unavailable. Select the token field and copy it manually.",
                  );
                }
              }}
            >
              Copy token
            </Button>
          </>
        )}
        <div className="git-actions">
          <Button
            variant="primary"
            disabled={pending}
            onClick={() => change("POST")}
          >
            {pending ? "Updating…" : "Generate token"}
          </Button>
          <Button disabled={pending} onClick={() => change("DELETE")}>
            Revoke tokens
          </Button>
        </div>
        {message && <p role="status">{message}</p>}
      </div>
    </details>
  );
}

export function CloneMenu({ repo }: { repo: Repository }) {
  const url = `${window.location.origin}/git/${encodeURIComponent(repo.owner)}/${encodeURIComponent(repo.name)}`;
  const [copied, setCopied] = useState(false);
  const [error, setError] = useState(false);
  return (
    <details className="clone-menu">
      <summary>Clone</summary>
      <div className="panel git-popover">
        <h2>Clone with HTTP</h2>
        <label htmlFor="clone-url">Repository URL</label>
        <input
          id="clone-url"
          readOnly
          value={url}
          onFocus={(event) => event.currentTarget.select()}
        />
        <Button
          onClick={async () => {
            try {
              await navigator.clipboard.writeText(url);
              setCopied(true);
              setError(false);
            } catch {
              setError(true);
            }
          }}
        >
          {copied ? "Copied" : "Copy URL"}
        </Button>
        <p>
          Run <code>git clone</code> with this URL. For team deployments,
          generate a token under Git access and use it when Git prompts for a
          password.
        </p>
        <p className="muted">
          Fetch uses protocol v2. Push is not available yet.
        </p>
        {error && (
          <p role="status">Select the URL field and copy it manually.</p>
        )}
      </div>
    </details>
  );
}
