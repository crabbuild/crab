import { useState } from "react";
import { Button } from "@primer/react";
import {
  CodeIcon,
  DownloadIcon,
  TriangleDownIcon,
} from "@primer/octicons-react";
import type { Icon } from "@primer/octicons-react";
import type { Repository, Session } from "./api";

interface GitToken {
  username: string;
  token: string;
  expires_in: number;
  owner: string;
  repository: string;
  access: "read" | "write";
}

export function GitAccess({
  session,
  repositories,
}: {
  session: Session;
  repositories: Repository[];
}) {
  const [token, setToken] = useState<GitToken>();
  const [selected, setSelected] = useState("");
  const [access, setAccess] = useState<"read" | "write">("read");
  const [pending, setPending] = useState(false);
  const [message, setMessage] = useState<string>();
  const credentialKey =
    `credential.${window.location.origin}.useHttpPath`.replaceAll("'", "'\\''");
  const repository = repositories.find(
    (repo) => `${repo.owner}/${repo.name}` === selected,
  );
  async function change(method: "POST" | "DELETE") {
    if (method === "POST" && !repository) return;
    setPending(true);
    setToken(undefined);
    setMessage(undefined);
    try {
      const response = await fetch("/api/git-token", {
        method,
        headers: {
          "X-CSRF-Token": session.csrf ?? "",
          "Content-Type": "application/json",
        },
        body:
          method === "POST" && repository
            ? JSON.stringify({
                owner: repository.owner,
                repository: repository.name,
                access,
              })
            : undefined,
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
          password. It grants the selected access to one repository and expires
          when this sign-in expires or you sign out.
        </p>
        <p>Save it in your Git credential manager. It is shown only here.</p>
        <details className="credential-setup">
          <summary>Credential manager setup</summary>
          <p>Run once to keep this server’s repository tokens separate:</p>
          <code>git config --global '{credentialKey}' true</code>
        </details>
        <label htmlFor="git-token-repository">Repository</label>
        <select
          id="git-token-repository"
          value={selected}
          disabled={pending}
          onChange={(event) => {
            setSelected(event.target.value);
            setAccess("read");
            setToken(undefined);
            setMessage(undefined);
          }}
        >
          <option value="">Choose a repository</option>
          {repositories.map((repo) => {
            const name = `${repo.owner}/${repo.name}`;
            return (
              <option key={name} value={name}>
                {name}
              </option>
            );
          })}
        </select>
        <label htmlFor="git-token-access">Access</label>
        <select
          id="git-token-access"
          value={access}
          disabled={pending || !repository}
          onChange={(event) => {
            setAccess(event.target.value === "write" ? "write" : "read");
            setToken(undefined);
            setMessage(undefined);
          }}
        >
          <option value="read">Read (clone and fetch)</option>
          {repository?.access === "write" && (
            <option value="write">Read and write (push)</option>
          )}
        </select>
        {repositories.length === 0 && (
          <p>No repositories available for a token.</p>
        )}
        {token && (
          <>
            <label htmlFor="git-token">
              {token.access === "write" ? "Write" : "Read"} token for{" "}
              {token.owner}/{token.repository} · expires in about{" "}
              {Math.ceil(token.expires_in / 60)} minutes
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
            disabled={pending || !repository}
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

export function CloneMenu({
  repo,
  revision,
  compact = false,
  icon: CompactIcon = CodeIcon,
  label = "Code",
}: {
  repo: Repository;
  revision: string;
  compact?: boolean;
  icon?: Icon;
  label?: string;
}) {
  const url = `${window.location.origin}/git/${encodeURIComponent(repo.owner)}/${encodeURIComponent(repo.name)}.git`;
  const archive = `/api/repos/${encodeURIComponent(repo.owner)}/${encodeURIComponent(repo.name)}/archive?${new URLSearchParams({ rev: revision })}`;
  const [copied, setCopied] = useState(false);
  const [error, setError] = useState(false);
  return (
    <details className={`clone-menu${compact ? " compact" : ""}`}>
      <summary aria-label={compact ? label : undefined}>
        {compact ? (
          <CompactIcon />
        ) : (
          <>
            <CodeIcon /> Code <TriangleDownIcon size={12} />
          </>
        )}
      </summary>
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
          Push requires write access. Non-fast-forward updates are rejected.
        </p>
        <a className="download-archive" href={archive}>
          <DownloadIcon /> Download ZIP
        </a>
        {error && (
          <p role="status">Select the URL field and copy it manually.</p>
        )}
      </div>
    </details>
  );
}
