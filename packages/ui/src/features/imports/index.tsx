import { Textarea } from "@/components/ui/textarea";
import { Input } from "@/components/ui/input";
import { Spinner } from "@/components/ui/spinner";
import { Button } from "@/components/ui/button";
import { FieldSelect } from "@/components/ui/field-select";
import { Field, FieldDescription, FieldLabel } from "@/components/ui/field";
import { useEffect, useRef, useState, type FormEvent } from "react";

import {
  AlertIcon,
  CheckCircleFillIcon,
  GitPullRequestIcon,
  RepoForkedIcon,
  RepoIcon,
  XIcon,
} from "@primer/octicons-react";
import {
  gitImportStatus,
  navigate,
  resumeGitImport,
  startGitImport,
  startRepositoryFork,
  useRequest,
  type GitImportJob,
  type Repository,
  type RepositoryVisibility,
} from "@/lib/api";
import { parseGitRepository } from "./source";
import { Link } from "@/components/navigation/link";

interface ImportedRepository {
  owner: string;
  name: string;
}

const visibilityLabels = { private: "Private", public: "Public" } as const;

export function GitImport({
  account,
  csrf,
  defaultOwner = "",
  personal = false,
  organization,
  fork,
  onImported,
}: {
  account: string;
  csrf: string;
  defaultOwner?: string;
  personal?: boolean;
  organization?: { id: string; name: string; requestedJob?: string };
  fork?: Pick<Repository, "id" | "owner" | "name" | "can_fork_to_organization">;
  onImported: (repository: ImportedRepository) => void;
}) {
  const organizationId = organization?.id;
  const organizationPage = organizationId
    ? `/-/organizations/${encodeURIComponent(organizationId)}?tab=repositories`
    : undefined;
  const storageKey = fork
    ? `crab:fork:${account}:${fork.id}`
    : organizationId
      ? `crab:git-import:${account}:${organizationId}`
      : personal
        ? `crab:git-import:personal:${account}`
        : `crab:git-import:${account}`;
  const [savedJob] = useState(() => {
    if (organization?.requestedJob) return organization.requestedJob;
    try {
      return sessionStorage.getItem(storageKey) ?? "";
    } catch {
      return "";
    }
  });
  const [jobId, setJobId] = useState(savedJob.replace(/^personal:/, ""));
  const defaultForkDestination = fork?.can_fork_to_organization
    ? "organization"
    : "personal";
  const [forkDestination, setForkDestination] = useState<
    "personal" | "organization"
  >(
    savedJob.startsWith("personal:")
      ? "personal"
      : savedJob && fork
        ? "organization"
        : defaultForkDestination,
  );
  const jobOrganization =
    fork && forkDestination === "personal" ? undefined : organizationId;
  const [open, setOpen] = useState(!!jobId);
  const personalDestination = useRequest<{
    owner: string;
    visibilities: RepositoryVisibility[];
  }>(personal && open ? "/api/repos/creation" : null);
  const [source, setSource] = useState("");
  const [owner, setOwner] = useState(defaultOwner);
  const defaultName = fork
    ? forkDestination === "personal"
      ? fork.name
      : `${fork.name}-fork`
    : "";
  const [name, setName] = useState(defaultName);
  const [description, setDescription] = useState("");
  const [visibility, setVisibility] = useState<RepositoryVisibility>("private");
  const [token, setToken] = useState("");
  const [nameEdited, setNameEdited] = useState(false);
  const [job, setJob] = useState<GitImportJob>();
  const forkJob = Boolean(fork || job?.fork_source);
  const operation = forkJob ? "fork" : "import";
  const visibilities: RepositoryVisibility[] = job
    ? [job.visibility]
    : organization
      ? ["private", "public"]
      : (personalDestination.data?.visibilities ?? ["private"]);
  const [error, setError] = useState<string>();
  const [submitting, setSubmitting] = useState(false);
  const [statusCheck, setStatusCheck] = useState(0);
  const [attempt, setAttempt] = useState<{ id: string; intent: string }>();
  const notifiedJob = useRef<string | undefined>(undefined);

  function remember(id: string) {
    setJobId(id);
    try {
      if (id)
        sessionStorage.setItem(
          storageKey,
          fork && forkDestination === "personal" ? `personal:${id}` : id,
        );
      else sessionStorage.removeItem(storageKey);
    } catch {
      // The active page can still show status when browser storage is disabled.
    }
  }

  useEffect(() => {
    if (
      !jobId ||
      submitting ||
      (job && job.state !== "queued" && job.state !== "running")
    )
      return;
    const controller = new AbortController();
    const timer = window.setTimeout(
      () => {
        void gitImportStatus(jobId, controller.signal, jobOrganization)
          .then((next) => {
            setError(undefined);
            setJob(next);
          })
          .catch((failure: unknown) => {
            if (!controller.signal.aborted) {
              setError(
                failure instanceof Error
                  ? failure.message
                  : `Could not check ${operation} progress`,
              );
              // Keep polling after a transient network failure; a single failed
              // status request must not strand an otherwise healthy import.
              setJob((current) => (current ? { ...current } : current));
            }
          });
      },
      job ? 1_500 : 0,
    );
    return () => {
      controller.abort();
      window.clearTimeout(timer);
    };
  }, [job, jobId, operation, jobOrganization, statusCheck, submitting]);

  useEffect(() => {
    if (
      job?.state === "succeeded" &&
      job.repository &&
      notifiedJob.current !== job.id
    ) {
      notifiedJob.current = job.id;
      try {
        sessionStorage.removeItem(storageKey);
      } catch {
        // The catalog can still open the imported repository.
      }
      onImported(job.repository);
    }
  }, [job, onImported, storageKey]);

  function reset() {
    remember("");
    if (organization?.requestedJob && organizationPage)
      navigate(organizationPage);
    setAttempt(undefined);
    setSource("");
    setOwner(defaultOwner);
    setName(defaultName);
    setDescription("");
    setVisibility("private");
    setToken("");
    setNameEdited(false);
    setJob(undefined);
    setError(undefined);
    setSubmitting(false);
  }

  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (job?.resumable) {
      setSubmitting(true);
      setError(undefined);
      try {
        setJob(
          await resumeGitImport(
            job.id,
            token || undefined,
            csrf,
            jobOrganization,
          ),
        );
      } catch (failure) {
        setError(
          failure instanceof Error
            ? failure.message
            : `Could not resume ${operation}`,
        );
      } finally {
        setToken("");
        setSubmitting(false);
      }
      return;
    }
    if (job) return;
    const sourceValue = source.trim();
    const parsedName = parseGitRepository(sourceValue);
    const repositoryName = name.trim() || parsedName;
    if (!fork && !sourceValue) {
      setError("Enter a Git source URL or a GitHub owner/name.");
      return;
    }
    const destination = personal
      ? personalDestination.data?.owner
      : owner.trim();
    if ((!fork && !organizationId && !destination) || !repositoryName) {
      setError("Choose the Crab owner and repository name.");
      return;
    }
    setSubmitting(true);
    setError(undefined);
    setJob(undefined);
    const fields = {
      source: sourceValue,
      owner: organizationId ? undefined : destination,
      name: repositoryName.trim(),
      description: description.trim() || undefined,
      visibility,
    };
    const intent = JSON.stringify([fields, fork ? forkDestination : null]);
    const request_id =
      attempt?.intent === intent && !job ? attempt.id : crypto.randomUUID();
    setAttempt({ id: request_id, intent });
    remember(request_id);
    try {
      const next = fork
        ? await startRepositoryFork(
            fork,
            { request_id, name: fields.name, destination: forkDestination },
            csrf,
          )
        : await startGitImport(
            {
              ...fields,
              request_id,
              token: token || undefined,
            },
            csrf,
            organizationId,
          );
      setJob(next);
    } catch (failure) {
      setError(
        failure instanceof Error
          ? failure.message
          : `Could not start ${operation}`,
      );
    } finally {
      setToken("");
      setSubmitting(false);
    }
  }

  if (!open)
    return (
      <Button variant="outline" onClick={() => setOpen(true)}>
        {fork ? (
          <RepoForkedIcon data-icon="inline-start" />
        ) : (
          <GitPullRequestIcon data-icon="inline-start" />
        )}
        {fork ? "Fork repository" : "Import from Git"}
      </Button>
    );

  const running =
    job?.state === "queued" ||
    job?.state === "running" ||
    (!!jobId && !job && !error);
  const importedHref = job?.repository
    ? `/${encodeURIComponent(job.repository.owner)}/${encodeURIComponent(job.repository.name)}`
    : undefined;
  const fixedDestination = !!job || running || submitting;
  return (
    <section className="panel git-import" aria-labelledby="git-import-title">
      <header className="git-import-header">
        <div>
          {!forkJob && <p className="git-import-eyebrow">REMOTE IMPORT</p>}
          <h2 id="git-import-title">
            {forkJob ? "Fork repository" : "Bring a repository from Git"}
          </h2>
          <p className="muted">
            {forkJob ? (
              "The fork stays private and uses the source repository’s access and rules."
            ) : (
              <>
                Crab copies Git history and refs from an allowed Git host into
                your object storage. An HTTPS token is only needed for private
                repositories and is never stored.
              </>
            )}
          </p>
        </div>
        <Button
          variant="outline"
          aria-label={forkJob ? "Close fork form" : "Close Git import"}
          onClick={() => {
            setOpen(false);
            setToken("");
          }}
        >
          <XIcon data-icon="inline-start" />
          Close
        </Button>
      </header>
      <form className="git-import-form" onSubmit={submit}>
        <div className="git-import-grid">
          {fork && (
            <Field>
              <FieldLabel htmlFor="fork-owner">Owner</FieldLabel>
              <FieldSelect
                id="fork-owner"
                label="Fork owner"
                value={forkDestination}
                disabled={fixedDestination || !!jobId}
                options={{
                  personal: "Personal",
                  ...(fork.can_fork_to_organization
                    ? { organization: organization?.name ?? fork.owner }
                    : {}),
                }}
                onValueChange={(value) => {
                  const next =
                    value === "organization" ? "organization" : "personal";
                  setForkDestination(next);
                  if (!nameEdited)
                    setName(
                      next === "personal" ? fork.name : `${fork.name}-fork`,
                    );
                }}
              />
            </Field>
          )}
          {!forkJob && (
            <label>
              Git source
              <Input
                required
                value={job?.source ?? source}
                onChange={(event) => {
                  const value = event.target.value;
                  setSource(value);
                  const parsedName = parseGitRepository(value);
                  if (!nameEdited && parsedName) setName(parsedName);
                }}
                placeholder="https://github.com/owner/repository.git"
                autoComplete="url"
                disabled={fixedDestination}
              />
            </label>
          )}
          {!forkJob && (
            <label>
              {organization
                ? "Organization"
                : personal
                  ? "Personal namespace"
                  : "Crab owner"}
              <Input
                required
                value={
                  organization?.name ??
                  job?.owner ??
                  (personal ? (personalDestination.data?.owner ?? "") : owner)
                }
                onChange={(event) => setOwner(event.target.value)}
                placeholder={
                  personalDestination.loading
                    ? "Loading namespace…"
                    : personal
                      ? undefined
                      : "team"
                }
                autoComplete="organization"
                disabled={!!organization || personal || fixedDestination}
              />
            </label>
          )}
          <label>
            Repository name
            <Input
              required
              value={job?.name ?? name}
              onChange={(event) => {
                setNameEdited(true);
                setName(event.target.value);
              }}
              placeholder="repository"
              autoComplete="off"
              disabled={fixedDestination}
            />
          </label>
          {!forkJob && (!job || job.resumable) && (
            <label>
              Source token <span className="muted">(HTTPS private repos)</span>
              <Input
                type="password"
                value={token}
                onChange={(event) => setToken(event.target.value)}
                placeholder="Optional personal access token"
                autoComplete="new-password"
                disabled={running || submitting}
              />
            </label>
          )}
          {!forkJob && (
            <Field>
              <FieldLabel htmlFor="git-import-visibility">
                Visibility
              </FieldLabel>
              <FieldSelect
                id="git-import-visibility"
                label="Visibility"
                aria-describedby="git-import-visibility-help"
                value={job?.visibility ?? visibility}
                disabled={fixedDestination || !!jobId}
                options={visibilities.map((value) => ({
                  value,
                  label: visibilityLabels[value],
                }))}
                onValueChange={(value) => {
                  setVisibility(value === "public" ? "public" : "private");
                }}
              />
              <FieldDescription id="git-import-visibility-help">
                {(job?.visibility ?? visibility) === "public"
                  ? "Anyone can view the imported code and commit history once setup finishes."
                  : "Only people with access can view the imported repository."}
              </FieldDescription>
            </Field>
          )}
          {!forkJob && !job && (
            <label className="git-import-description">
              Description <span className="muted">(optional)</span>
              <Textarea
                value={description}
                onChange={(event) => setDescription(event.target.value)}
                placeholder="What should people know about this repository?"
                rows={2}
                disabled={running || submitting}
              />
            </label>
          )}
        </div>
        {personalDestination.error && !job && (
          <div className="git-import-feedback error" role="alert">
            <span>{personalDestination.error}</span>
            <Button
              type="button"
              variant="outline"
              size="sm"
              onClick={personalDestination.retry}
            >
              Retry namespace lookup
            </Button>
          </div>
        )}
        {error && (
          <div className="git-import-feedback error" role="alert">
            <AlertIcon aria-hidden="true" />
            <span>{error}</span>
          </div>
        )}
        {!!jobId && !job && !error && (
          <div className="git-import-feedback" role="status">
            <Spinner /> Checking {operation} progress…
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
            <code>
              {jobOrganization ? (organization?.name ?? job.owner) : job.owner}/
              {job.name}
            </code>
            {jobOrganization && organizationPage && !importedHref && (
              <Link
                href={`${organizationPage}&import=${encodeURIComponent(job.id)}`}
                className="git-import-result"
              >
                {forkJob ? "Fork status link" : "Import status link"}
              </Link>
            )}
            {importedHref && (
              <Link href={importedHref} className="git-import-result">
                <RepoIcon aria-hidden="true" /> Open repository
              </Link>
            )}
          </div>
        )}
        <div className="git-import-actions">
          {((jobId && !job && error) || job?.resumable) && (
            <Button
              type="button"
              variant="outline"
              onClick={() => {
                setJob(undefined);
                setError(undefined);
                setStatusCheck((value) => value + 1);
              }}
            >
              Check {operation} status
            </Button>
          )}
          {(!job || running || job.resumable) && (
            <Button
              type="submit"
              disabled={
                running ||
                submitting ||
                (personal && !job && !personalDestination.data)
              }
            >
              {submitting || running
                ? forkJob
                  ? "Creating fork…"
                  : "Importing…"
                : job?.resumable
                  ? forkJob
                    ? "Resume fork"
                    : "Resume import"
                  : forkJob
                    ? "Create fork"
                    : "Start import"}
            </Button>
          )}
          {(job || (organization?.requestedJob && error)) && !running && (
            <Button
              variant="outline"
              type="button"
              onClick={reset}
              disabled={submitting}
            >
              {fork ? "Start another fork" : "Start another import"}
            </Button>
          )}
        </div>
      </form>
    </section>
  );
}
