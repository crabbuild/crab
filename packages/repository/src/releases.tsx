import { useState } from "react";
import { Button, IconButton, Label, TextInput } from "@primer/react";
import {
  ChevronLeftIcon,
  DownloadIcon,
  GitCommitIcon,
  PencilIcon,
  SearchIcon,
  TagIcon,
  TrashIcon,
} from "@primer/octicons-react";
import {
  endpoint,
  navigate,
  repoHref,
  useRequest,
  type Refs,
  type Repository,
} from "./api";
import { DiscussionMarkdown } from "./discussion";
import { EditRelease, NewRelease } from "./release-form";
import { mutateRelease, type Release, type ReleasePage } from "./release-api";
import { Link, Result, short } from "./ui";

function timestamp(value: number) {
  return new Date(value).toLocaleString(undefined, {
    dateStyle: "medium",
    timeStyle: "short",
  });
}

function ReleaseNavigation({
  repo,
  query,
}: {
  repo: Repository;
  query?: string;
}) {
  const [search, setSearch] = useState(query ?? "");
  return (
    <div className="release-navigation">
      <nav className="release-tabs" aria-label="Releases and tags">
        <Link
          className="active"
          aria-current="page"
          href={repoHref(repo, { view: "releases" })}
        >
          Releases
        </Link>
        <Link href={repoHref(repo, { view: "tags" })}>Tags</Link>
      </nav>
      {query !== undefined && (
        <form
          role="search"
          aria-label="Search releases"
          onSubmit={(event) => {
            event.preventDefault();
            navigate(
              repoHref(repo, {
                view: "releases",
                query: search.trim() || undefined,
              }),
            );
          }}
        >
          <TextInput
            aria-label="Find a release"
            leadingVisual={SearchIcon}
            placeholder="Find a release"
            value={search}
            maxLength={256}
            onChange={(event) => setSearch(event.target.value)}
          />
        </form>
      )}
    </div>
  );
}

function ReleaseMeta({ release }: { release: Release }) {
  const occurredAt = release.draft
    ? release.updated_at
    : (release.published_at ?? release.created_at);
  return (
    <p className="release-meta muted">
      <strong>{release.author}</strong>{" "}
      {release.draft ? "saved this draft" : "released this"} on{" "}
      <time dateTime={new Date(occurredAt).toISOString()}>
        {timestamp(occurredAt)}
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
    <details className="release-assets" open>
      <summary>
        Assets <span>1</span>
      </summary>
      <a
        href={endpoint(repo, "archive", {
          rev: `refs/tags/${release.tag_name}`,
        })}
        download={`${repo.name}-${release.tag_name}.zip`}
      >
        <DownloadIcon /> Source code (zip)
      </a>
    </details>
  );
}

function ReleaseCard({
  repo,
  release,
  manage,
}: {
  repo: Repository;
  release: Release;
  manage?: { edit: () => void; remove: () => Promise<void> };
}) {
  const [confirming, setConfirming] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const [error, setError] = useState<string>();
  return (
    <article className="panel release-card">
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
            <div className="release-ref-meta">
              <TagIcon />
              {release.draft ? (
                <span>{release.tag_name}</span>
              ) : (
                <Link href={repoHref(repo, { view: "tags" })}>
                  {release.tag_name}
                </Link>
              )}
              <GitCommitIcon />
              <Link
                href={repoHref(repo, {
                  view: "commit",
                  rev: release.target_oid,
                })}
              >
                {short(release.target_oid)}
              </Link>
            </div>
          </div>
          <div className="release-header-actions">
            {release.draft && <Label variant="accent">Draft</Label>}
            {release.prerelease && (
              <Label variant="attention">Pre-release</Label>
            )}
            {manage && (
              <>
                <IconButton
                  icon={PencilIcon}
                  aria-label={`Edit ${release.title}`}
                  title="Edit release"
                  size="small"
                  variant="invisible"
                  onClick={manage.edit}
                />
                <IconButton
                  icon={TrashIcon}
                  aria-label={`Delete ${release.title}`}
                  title="Delete release"
                  size="small"
                  variant="danger"
                  onClick={() => {
                    setError(undefined);
                    setConfirming(true);
                  }}
                />
              </>
            )}
          </div>
        </header>
        {confirming && manage && (
          <div className="release-delete-confirm">
            <div>
              <strong>Delete this release?</strong>
              <span>
                The release notes will be removed.{" "}
                {release.draft
                  ? "No Git tag will be published."
                  : `Tag ${release.tag_name} will remain available to Git clients.`}
              </span>
              {error && (
                <span className="release-delete-error" role="alert">
                  {error}
                </span>
              )}
            </div>
            <div>
              <Button
                size="small"
                disabled={deleting}
                onClick={() => {
                  setConfirming(false);
                  setError(undefined);
                }}
              >
                Cancel
              </Button>
              <Button
                size="small"
                variant="danger"
                disabled={deleting}
                onClick={async () => {
                  setDeleting(true);
                  setError(undefined);
                  try {
                    await manage.remove();
                  } catch (failure) {
                    setError(
                      failure instanceof Error
                        ? failure.message
                        : "The release could not be deleted",
                    );
                    setDeleting(false);
                  }
                }}
              >
                {deleting ? "Deleting…" : "Delete this release"}
              </Button>
            </div>
          </div>
        )}
        <DiscussionMarkdown>{release.body}</DiscussionMarkdown>
        {!release.draft && (
          <footer>
            <SourceArchive repo={repo} release={release} />
          </footer>
        )}
      </div>
    </article>
  );
}

function ReleaseList({
  repo,
  before,
  csrf,
  canManage,
  query,
}: {
  repo: Repository;
  before?: string;
  csrf: string;
  canManage: boolean;
  query: string;
}) {
  const releases = useRequest<ReleasePage>(
    endpoint(repo, "releases", { before, limit: "20", query }),
  );
  return (
    <Result state={releases} showTiming={false}>
      {(page) => (
        <>
          {page.items.length ? (
            <div className="release-workspace">
              <aside className="release-index" aria-label="Release list">
                <h3>Release list</h3>
                {page.items.map((release, index) => (
                  <Link
                    key={release.number}
                    className={index === 0 ? "active" : undefined}
                    href={repoHref(repo, {
                      view: "releases",
                      release: String(release.number),
                    })}
                  >
                    {release.tag_name}
                  </Link>
                ))}
              </aside>
              <div className="release-list">
                {page.items.map((release) => (
                  <ReleaseCard
                    key={release.number}
                    repo={repo}
                    release={release}
                    manage={
                      canManage
                        ? {
                            edit: () =>
                              navigate(
                                repoHref(repo, {
                                  view: "releases",
                                  release: String(release.number),
                                  action: "edit",
                                }),
                              ),
                            remove: async () => {
                              await mutateRelease(
                                repo,
                                csrf,
                                release,
                                "DELETE",
                                { version: release.version },
                              );
                              releases.retry();
                            },
                          }
                        : undefined
                    }
                  />
                ))}
              </div>
            </div>
          ) : (
            <div className="notice release-empty">
              <TagIcon size={24} />
              <h3>There aren’t any releases here</h3>
              <p>
                {query
                  ? "No release tag, title, notes, or author matches this search."
                  : "Published releases and their source archives will appear here."}
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
                  query: query || undefined,
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

function ReleaseDetail({
  repo,
  number,
  csrf,
  canManage,
}: {
  repo: Repository;
  number: string;
  csrf: string;
  canManage: boolean;
}) {
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
          <ReleaseCard
            repo={repo}
            release={current}
            manage={
              canManage
                ? {
                    edit: () =>
                      navigate(
                        repoHref(repo, {
                          view: "releases",
                          release: number,
                          action: "edit",
                        }),
                      ),
                    remove: async () => {
                      await mutateRelease(repo, csrf, current, "DELETE", {
                        version: current.version,
                      });
                      navigate(repoHref(repo, { view: "releases" }));
                    },
                  }
                : undefined
            }
          />
        </div>
      )}
    </Result>
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
  const canManage = repo.access === "write" && !repo.archived;
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
      <ReleaseNavigation
        repo={repo}
        query={selected ? undefined : (url.searchParams.get("query") ?? "")}
      />
      {selected && url.searchParams.get("action") === "edit" ? (
        canManage ? (
          <EditRelease
            repo={repo}
            number={selected}
            csrf={csrf}
            onPublished={onPublished}
          />
        ) : (
          <div className="notice error" role="alert">
            Write access to an active repository is required to edit a release.
          </div>
        )
      ) : selected ? (
        <ReleaseDetail
          repo={repo}
          number={selected}
          csrf={csrf}
          canManage={canManage}
        />
      ) : (
        <ReleaseList
          repo={repo}
          before={url.searchParams.get("before") ?? undefined}
          csrf={csrf}
          canManage={canManage}
          query={url.searchParams.get("query") ?? ""}
        />
      )}
    </section>
  );
}
