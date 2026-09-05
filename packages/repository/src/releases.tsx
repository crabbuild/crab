import { useMemo, useState } from "react";
import { Button, Label, TextInput } from "@primer/react";
import { ChevronLeftIcon, DownloadIcon, TagIcon } from "@primer/octicons-react";
import {
  endpoint,
  navigate,
  repoHref,
  useRequest,
  type Ref,
  type Refs,
  type Repository,
} from "./api";
import {
  DiscussionMarkdown,
  Editor,
  Failure,
  useSubmission,
} from "./discussion";
import { Link, Result, short } from "./ui";

interface Release {
  number: number;
  tag_name: string;
  tag_oid: string;
  target_oid: string;
  title: string;
  body: string;
  prerelease: boolean;
  author: string;
  created_at: number;
}

interface ReleasePage {
  items: Release[];
  next: number | null;
}

const refName = (ref: Ref) => ref.name.replace(/^refs\/(?:heads|tags)\//, "");

function timestamp(value: number) {
  return new Date(value).toLocaleString(undefined, {
    dateStyle: "medium",
    timeStyle: "short",
  });
}

function ReleaseNavigation({ repo }: { repo: Repository }) {
  return (
    <nav className="refs-tabs release-tabs" aria-label="Releases and tags">
      <Link
        className="active"
        aria-current="page"
        href={repoHref(repo, { view: "releases" })}
      >
        Releases
      </Link>
      <Link href={repoHref(repo, { view: "tags" })}>Tags</Link>
    </nav>
  );
}

function ReleaseMeta({ release }: { release: Release }) {
  return (
    <p className="release-meta muted">
      <strong>{release.author}</strong> released this on{" "}
      <time dateTime={new Date(release.created_at).toISOString()}>
        {timestamp(release.created_at)}
      </time>
    </p>
  );
}

function SourceArchive({
  repo,
  release,
}: {
  repo: Repository;
  release: Release;
}) {
  return (
    <a
      className="button-link release-archive"
      href={endpoint(repo, "archive", {
        rev: `refs/tags/${release.tag_name}`,
      })}
      download={`${repo.name}-${release.tag_name}.zip`}
    >
      <DownloadIcon /> Source code (zip)
    </a>
  );
}

function ReleaseCard({
  repo,
  release,
}: {
  repo: Repository;
  release: Release;
}) {
  return (
    <article className="panel release-card">
      <aside className="release-tag">
        <TagIcon />
        <Link href={repoHref(repo, { view: "tags" })}>{release.tag_name}</Link>
        <Link
          className="release-target"
          href={repoHref(repo, { view: "commit", rev: release.target_oid })}
        >
          {short(release.target_oid)}
        </Link>
      </aside>
      <div className="release-content">
        <header>
          <div>
            <h3>
              <Link
                href={repoHref(repo, {
                  view: "releases",
                  release: String(release.number),
                })}
              >
                {release.title}
              </Link>
            </h3>
            <ReleaseMeta release={release} />
          </div>
          {release.prerelease && <Label variant="attention">Pre-release</Label>}
        </header>
        <DiscussionMarkdown>{release.body}</DiscussionMarkdown>
        <footer>
          <SourceArchive repo={repo} release={release} />
        </footer>
      </div>
    </article>
  );
}

function ReleaseList({ repo, before }: { repo: Repository; before?: string }) {
  const releases = useRequest<ReleasePage>(
    endpoint(repo, "releases", { before, limit: "20" }),
  );
  return (
    <Result state={releases} showTiming={false}>
      {(page) => (
        <>
          {page.items.length ? (
            <div className="release-list">
              {page.items.map((release) => (
                <ReleaseCard
                  key={release.number}
                  repo={repo}
                  release={release}
                />
              ))}
            </div>
          ) : (
            <div className="notice release-empty">
              <TagIcon size={24} />
              <h3>There aren’t any releases here</h3>
              <p>
                Published releases and their source archives will appear here.
              </p>
            </div>
          )}
          {page.next && (
            <nav className="release-pagination" aria-label="Release pages">
              <Link
                className="button-link"
                href={repoHref(repo, {
                  view: "releases",
                  before: String(page.next),
                })}
              >
                Older releases
              </Link>
            </nav>
          )}
        </>
      )}
    </Result>
  );
}

function ReleaseDetail({ repo, number }: { repo: Repository; number: string }) {
  const release = useRequest<Release>(endpoint(repo, `releases/${number}`));
  return (
    <Result state={release} showTiming={false}>
      {(current) => (
        <div className="release-detail">
          <Link
            className="release-back"
            href={repoHref(repo, { view: "releases" })}
          >
            <ChevronLeftIcon /> Releases
          </Link>
          <ReleaseCard repo={repo} release={current} />
        </div>
      )}
    </Result>
  );
}

function NewRelease({
  repo,
  refs,
  csrf,
  onPublished,
}: {
  repo: Repository;
  refs: Refs;
  csrf: string;
  onPublished: () => void;
}) {
  const branches = useMemo(
    () => refs.refs.filter((ref) => ref.name.startsWith("refs/heads/")),
    [refs.refs],
  );
  const tags = useMemo(
    () => refs.refs.filter((ref) => ref.name.startsWith("refs/tags/")),
    [refs.refs],
  );
  const [tag, setTag] = useState("");
  const [branch, setBranch] = useState(
    refs.head?.name ?? branches[0]?.name ?? "",
  );
  const [title, setTitle] = useState("");
  const [body, setBody] = useState("");
  const [prerelease, setPrerelease] = useState(false);
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string>();
  const submission = useSubmission();
  const existingTag = tags.find((ref) => refName(ref) === tag.trim());
  const selectedBranch = branches.find((ref) => ref.name === branch);
  const target = existingTag?.peeled ?? existingTag?.oid ?? selectedBranch?.oid;

  if (repo.archived || repo.access !== "write")
    return (
      <div className="notice error" role="alert">
        Write access to an active repository is required to publish a release.
      </div>
    );

  return (
    <form
      className="new-release"
      onSubmit={async (event) => {
        event.preventDefault();
        if (!target) return;
        const input = {
          tag_name: tag.trim(),
          target_oid: target,
          title,
          body,
          prerelease,
        };
        setPending(true);
        setError(undefined);
        try {
          const response = await fetch(endpoint(repo, "releases"), {
            method: "POST",
            headers: {
              Accept: "application/json",
              "Content-Type": "application/json",
              "X-CSRF-Token": csrf,
            },
            body: JSON.stringify({ request_id: submission(input), ...input }),
          });
          if (response.status === 401)
            window.dispatchEvent(new Event("crab-session-expired"));
          const result = (await response.json()) as Release & {
            error?: { message?: string };
          };
          if (!response.ok)
            throw new Error(
              result.error?.message ?? `Request failed (${response.status})`,
            );
          onPublished();
          navigate(
            repoHref(repo, {
              view: "releases",
              release: String(result.number),
            }),
          );
        } catch (failure) {
          setError(
            failure instanceof Error
              ? failure.message
              : "The release could not be published",
          );
          setPending(false);
        }
      }}
    >
      <div className="new-release-heading">
        <div>
          <h2>New release</h2>
          <p>Publish a Git tag with release notes and a source archive.</p>
        </div>
        <TagIcon size={24} />
      </div>
      <fieldset className="release-target-picker">
        <legend>Choose a tag</legend>
        <label htmlFor="release-tag">Tag name</label>
        <TextInput
          id="release-tag"
          value={tag}
          onChange={(event) => setTag(event.target.value)}
          placeholder="v1.0.0"
          maxLength={255}
          required
          disabled={pending}
          autoFocus
        />
        <label htmlFor="release-target">Target</label>
        <select
          id="release-target"
          value={existingTag ? "existing-tag" : branch}
          onChange={(event) => setBranch(event.target.value)}
          disabled={pending || Boolean(existingTag)}
          required
        >
          {existingTag && (
            <option value="existing-tag">
              {refName(existingTag)} ({short(target ?? "")})
            </option>
          )}
          {branches.map((ref) => (
            <option key={ref.name} value={ref.name}>
              {refName(ref)} ({short(ref.oid)})
            </option>
          ))}
        </select>
        {existingTag && (
          <p className="muted release-existing-tag">
            Existing tag <strong>{refName(existingTag)}</strong> targets{" "}
            <code>{short(target ?? "")}</code>.
          </p>
        )}
      </fieldset>
      <div className="release-notes-form">
        <label htmlFor="release-title">Release title</label>
        <TextInput
          id="release-title"
          value={title}
          onChange={(event) => setTitle(event.target.value)}
          placeholder="Release title"
          maxLength={256}
          required
          disabled={pending}
        />
        <Editor
          id="release-notes"
          label="Release notes"
          value={body}
          onChange={setBody}
          disabled={pending}
        />
      </div>
      <label className="release-prerelease">
        <input
          type="checkbox"
          checked={prerelease}
          onChange={(event) => setPrerelease(event.target.checked)}
          disabled={pending}
        />
        <span>
          <strong>Set as a pre-release</strong>
          <small>
            This release may be unstable and not ready for production.
          </small>
        </span>
      </label>
      <Failure message={error} />
      <div className="release-form-actions">
        <Button
          type="button"
          onClick={() => navigate(repoHref(repo, { view: "releases" }))}
          disabled={pending}
        >
          Cancel
        </Button>
        <Button type="submit" variant="primary" disabled={pending || !target}>
          {pending ? "Publishing…" : "Publish release"}
        </Button>
      </div>
    </form>
  );
}

export function Releases({
  repo,
  refs,
  url,
  csrf,
  onPublished,
}: {
  repo: Repository;
  refs: Refs;
  url: URL;
  csrf: string;
  onPublished: () => void;
}) {
  const selected = url.searchParams.get("release");
  if (selected === "new")
    return (
      <NewRelease
        repo={repo}
        refs={refs}
        csrf={csrf}
        onPublished={onPublished}
      />
    );
  return (
    <section className="releases-page">
      <div className="releases-heading">
        <h2>Releases</h2>
        {repo.access === "write" && !repo.archived && (
          <Link
            className="button-link primary"
            href={repoHref(repo, { view: "releases", release: "new" })}
          >
            Draft a new release
          </Link>
        )}
      </div>
      <ReleaseNavigation repo={repo} />
      {selected ? (
        <ReleaseDetail repo={repo} number={selected} />
      ) : (
        <ReleaseList
          repo={repo}
          before={url.searchParams.get("before") ?? undefined}
        />
      )}
    </section>
  );
}
