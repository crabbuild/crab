import { Disclosure } from "@/components/ui/collapsible";
import { Input } from "@/components/ui/input";
import { FieldSelect } from "@/components/ui/field-select";
import { useState } from "react";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import {
  CodeIcon,
  DownloadIcon,
  KeyIcon,
  TriangleDownIcon,
} from "@primer/octicons-react";
import type { Icon } from "@primer/octicons-react";
import type { Session } from "@/lib/contracts";
import type { Repository } from "@/lib/api";

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
  onOpenChange,
}: {
  session: Session;
  repositories: Repository[];
  onOpenChange: (open: boolean) => void;
}) {
  const [token, setToken] = useState<GitToken>();
  const [selected, setSelected] = useState("");
  const [access, setAccess] = useState<"read" | "write">("read");
  const [closeRelatedIssues, setCloseRelatedIssues] = useState(false);
  const [pending, setPending] = useState(false);
  const [message, setMessage] = useState<string>();
  const credentialKey =
    `credential.${window.location.origin}.useHttpPath`.replaceAll("'", "'\\''");
  const repository = repositories.find(
    (repo) => `${repo.owner}/${repo.name}` === selected,
  );
  const requestedAccess =
    repository?.access === "write" && !repository.archived ? access : "read";
  const canCloseRelatedIssues =
    requestedAccess === "write" && !!repository?.organization_id;
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
                access: requestedAccess,
                ...(canCloseRelatedIssues && closeRelatedIssues
                  ? { close_related_issues: true }
                  : {}),
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
    <Disclosure
      className="git-access"
      onOpenChange={(open) => onOpenChange(open)}
      summaryLabel="Git access"
      indicator={false}
      summary={
        <>
          <KeyIcon className="git-access-icon" aria-hidden="true" />
          <span className="git-access-label">Git access</span>
        </>
      }
    >
      <div className="panel git-popover">
        <h2>Git access token</h2>
        <p>
          Use <code>crab</code> as your Git username and this token as the
          password. It grants the selected access to one repository and expires
          when this sign-in expires or you sign out.
        </p>
        <p>Save it in your Git credential manager. It is shown only here.</p>
        <Disclosure
          className="credential-setup"
          summary={<>Credential manager setup</>}
        >
          <p>Run once to keep this server’s repository tokens separate:</p>
          <code>git config --global '{credentialKey}' true</code>
        </Disclosure>
        <label htmlFor="git-token-repository">Repository</label>
        <FieldSelect
          id="git-token-repository"
          value={selected}
          disabled={pending}
          onValueChange={(nextValue) => {
            setSelected(nextValue);
            setAccess("read");
            setCloseRelatedIssues(false);
            setToken(undefined);
            setMessage(undefined);
          }}
          options={[
            { value: "", label: "Choose a repository" },
            ...repositories.map((repo) => {
              const name = `${repo.owner}/${repo.name}`;
              return { value: name, label: name };
            }),
          ]}
        />
        <label htmlFor="git-token-access">Access</label>
        <FieldSelect
          id="git-token-access"
          value={requestedAccess}
          disabled={pending || !repository}
          onValueChange={(nextValue) => {
            setAccess(nextValue === "write" ? "write" : "read");
            setCloseRelatedIssues(false);
            setToken(undefined);
            setMessage(undefined);
          }}
          options={[
            { value: "read", label: "Read (clone and fetch)" },
            ...(repository?.access === "write" && !repository.archived
              ? [{ value: "write", label: "Read and write (push)" }]
              : []),
          ]}
        />
        {canCloseRelatedIssues && (
          <label
            htmlFor="git-token-close-related-issues"
            className="flex items-start gap-2"
          >
            <Checkbox
              id="git-token-close-related-issues"
              checked={closeRelatedIssues}
              disabled={pending}
              onCheckedChange={(checked) => {
                setCloseRelatedIssues(checked === true);
                setToken(undefined);
                setMessage(undefined);
              }}
            />
            <span>
              Allow commit messages to close issues in this organization where
              you have write access
            </span>
          </label>
        )}
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
            <Input
              id="git-token"
              readOnly
              type="password"
              value={token.token}
              onFocus={(event) => event.currentTarget.select()}
              autoComplete="off"
            />
            <Button
              variant="outline"
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
            variant="default"
            disabled={pending || !repository}
            onClick={() => change("POST")}
          >
            {pending ? "Updating…" : "Generate token"}
          </Button>
          <Button
            variant="outline"
            disabled={pending}
            onClick={() => change("DELETE")}
          >
            Revoke tokens
          </Button>
        </div>
        {message && <p role="status">{message}</p>}
      </div>
    </Disclosure>
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
    <Disclosure
      className={`clone-menu${compact ? " compact" : ""}`}
      summaryLabel={compact ? label : undefined}
      indicator={false}
      summary={
        <>
          {compact ? (
            <CompactIcon />
          ) : (
            <>
              <CodeIcon /> Code <TriangleDownIcon size={12} />
            </>
          )}
        </>
      }
    >
      <div className="panel git-popover">
        <h2>Clone with HTTP</h2>
        <label htmlFor="clone-url">Repository URL</label>
        <Input
          id="clone-url"
          readOnly
          value={url}
          onFocus={(event) => event.currentTarget.select()}
        />
        <Button
          variant="outline"
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
          Run <code>git clone</code> with this URL.{" "}
          {repo.visibility === "public"
            ? "Public Git history can be cloned without signing in. Git LFS transfers require repository access."
            : "For team deployments, generate a token under Git access and use it when Git prompts for a password."}
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
    </Disclosure>
  );
}
