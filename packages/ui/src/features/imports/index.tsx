import { Spinner } from "@/components/ui/spinner";
import { Button } from "@/components/ui/button";
import { useEffect, useRef, useState, type FormEvent } from "react";

import {
  AlertIcon,
  CheckCircleFillIcon,
  GitPullRequestIcon,
  RepoIcon,
  XIcon,
} from "@primer/octicons-react";
import { gitImportStatus, startGitImport, type GitImportJob } from "@/lib/api";
import { parseGitRepository } from "./source";
import { Link } from "@/components/navigation/link";

interface ImportedRepository {
  owner: string;
  name: string;
}

export function GitImport({
  csrf,
  defaultOwner,
  onImported,
}: {
  csrf: string;
  defaultOwner: string;
  onImported: (repository: ImportedRepository) => void;
}) {
  const [open, setOpen] = useState(false);
  const [source, setSource] = useState("");
  const [owner, setOwner] = useState(defaultOwner);
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [token, setToken] = useState("");
  const [nameEdited, setNameEdited] = useState(false);
  const [job, setJob] = useState<GitImportJob>();
  const [error, setError] = useState<string>();
  const [submitting, setSubmitting] = useState(false);
  const notifiedJob = useRef<string | undefined>(undefined);

  useEffect(() => {
    if (!job || job.state === "succeeded" || job.state === "failed") return;
    const controller = new AbortController();
    const timer = window.setTimeout(() => {
      void gitImportStatus(job.id, controller.signal)
        .then((next) => {
          setError(undefined);
          setJob(next);
        })
        .catch((failure: unknown) => {
          if (!controller.signal.aborted) {
            setError(
              failure instanceof Error
                ? failure.message
                : "Could not check import progress",
            );
            // Keep polling after a transient network failure; a single failed
            // status request must not strand an otherwise healthy import.
            setJob((current) => (current ? { ...current } : current));
          }
        });
    }, 1_500);
    return () => {
      controller.abort();
      window.clearTimeout(timer);
    };
  }, [job]);

  useEffect(() => {
    if (
      job?.state === "succeeded" &&
      job.repository &&
      notifiedJob.current !== job.id
    ) {
      notifiedJob.current = job.id;
      onImported(job.repository);
    }
  }, [job, onImported]);

  function reset() {
    setOpen(false);
    setSource("");
    setOwner(defaultOwner);
    setName("");
    setDescription("");
    setToken("");
    setNameEdited(false);
    setJob(undefined);
    setError(undefined);
    setSubmitting(false);
  }

  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const sourceValue = source.trim();
    const parsedName = parseGitRepository(sourceValue);
    const repositoryName = name.trim() || parsedName;
    if (!sourceValue) {
      setError("Enter a Git source URL or a GitHub owner/name.");
      return;
    }
    if (!owner.trim() || !repositoryName) {
      setError("Choose the Crab owner and repository name.");
      return;
    }
    setSubmitting(true);
    setError(undefined);
    setJob(undefined);
    try {
      const next = await startGitImport(
        {
          source: sourceValue,
          owner: owner.trim(),
          name: repositoryName.trim(),
          description: description.trim() || undefined,
          token: token || undefined,
        },
        csrf,
      );
      setToken("");
      setJob(next);
    } catch (failure) {
      setError(
        failure instanceof Error ? failure.message : "Could not start import",
      );
    } finally {
      setSubmitting(false);
    }
  }

  if (!open)
    return (
      <Button variant="outline" onClick={() => setOpen(true)}>
        <GitPullRequestIcon data-icon="inline-start" />
        Import from Git
      </Button>
    );

  const running = job?.state === "queued" || job?.state === "running";
  const importedHref = job?.repository
    ? `/${encodeURIComponent(job.repository.owner)}/${encodeURIComponent(job.repository.name)}`
    : undefined;
  return (
    <section className="panel git-import" aria-labelledby="git-import-title">
      <header className="git-import-header">
        <div>
          <p className="git-import-eyebrow">REMOTE IMPORT</p>
          <h2 id="git-import-title">Bring a repository from Git</h2>
          <p className="muted">
            Crab copies Git history and refs from an allowed Git host into your
            object storage. An HTTPS token is only needed for private
            repositories and is never stored.
          </p>
        </div>
        <Button variant="outline" aria-label="Close Git import" onClick={reset}>
          <XIcon data-icon="inline-start" />
          Close
        </Button>
      </header>
      <form className="git-import-form" onSubmit={submit}>
        <div className="git-import-grid">
          <label>
            Git source
            <input
              required
              value={source}
              onChange={(event) => {
                const value = event.target.value;
                setSource(value);
                const parsedName = parseGitRepository(value);
                if (!nameEdited && parsedName) setName(parsedName);
              }}
              placeholder="https://github.com/owner/repository.git"
              autoComplete="url"
              disabled={running || submitting}
            />
          </label>
          <label>
            Crab owner
            <input
              required
              value={owner}
              onChange={(event) => setOwner(event.target.value)}
              placeholder="team"
              autoComplete="organization"
              disabled={running || submitting}
            />
          </label>
          <label>
            Repository name
            <input
              required
              value={name}
              onChange={(event) => {
                setNameEdited(true);
                setName(event.target.value);
              }}
              placeholder="repository"
              autoComplete="off"
              disabled={running || submitting}
            />
          </label>
          <label>
            Source token <span className="muted">(HTTPS private repos)</span>
            <input
              type="password"
              value={token}
              onChange={(event) => setToken(event.target.value)}
              placeholder="Optional personal access token"
              autoComplete="new-password"
              disabled={running || submitting}
            />
          </label>
          <label className="git-import-description">
            Description <span className="muted">(optional)</span>
            <textarea
              value={description}
              onChange={(event) => setDescription(event.target.value)}
              placeholder="What should people know about this repository?"
              rows={2}
              disabled={running || submitting}
            />
          </label>
        </div>
        {error && (
          <div className="git-import-feedback error" role="alert">
            <AlertIcon aria-hidden="true" />
            <span>{error}</span>
          </div>
        )}
        {job && (
          <div
            className={`git-import-feedback git-import-${job.state}`}
            role="status"
            aria-live="polite"
          >
            {running ? (
              <Spinner />
            ) : job.state === "succeeded" ? (
              <CheckCircleFillIcon aria-hidden="true" />
            ) : (
              <AlertIcon aria-hidden="true" />
            )}
            <span>{job.message}</span>
            {importedHref && (
              <Link href={importedHref} className="git-import-result">
                <RepoIcon aria-hidden="true" /> Open repository
              </Link>
            )}
          </div>
        )}
        <div className="git-import-actions">
          <Button
            type="submit"
            variant="default"
            disabled={running || submitting}
          >
            {submitting || running ? "Importing…" : "Start import"}
          </Button>
          <Button
            variant="outline"
            type="button"
            onClick={reset}
            disabled={running}
          >
            Cancel
          </Button>
        </div>
      </form>
    </section>
  );
}
