import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import { Spinner } from "@/components/ui/spinner";
import { endpoint, useRequest, type Commit, type Repository } from "@/lib/api";
import { date, short } from "@/lib/format";
import { repoHref } from "@/lib/repository-route";
import { AlertIcon, HistoryIcon } from "@primer/octicons-react";

export function LatestCommit({
  repo,
  rev,
  revision,
  path,
  kind,
}: {
  repo: Repository;
  rev: string;
  revision: string;
  path?: string;
  kind?: string;
}) {
  const historyHref = repoHref(repo, {
    view: "commits",
    rev: revision,
    path: path || undefined,
    kind: path ? kind : undefined,
  });
  const state = useRequest<Commit>(
    endpoint(repo, "commit", { rev, path_hex: path || undefined }),
  );
  if (state.loading || (!state.data && !state.error))
    return (
      <div className="latest-commit" role="status">
        <Spinner />
        <span className="muted">Loading latest commit…</span>
      </div>
    );
  if (state.error)
    return (
      <div className="latest-commit latest-commit-error" role="alert">
        <AlertIcon aria-hidden="true" />
        <span className="truncate">
          <strong>Latest commit unavailable.</strong> {state.error}
        </span>
        <Button
          variant="outline"
          size="sm"
          aria-label="Retry latest commit"
          onClick={state.retry}
        >
          Retry
        </Button>
        <Link className="commit-history-link" href={historyHref}>
          <HistoryIcon /> History
        </Link>
      </div>
    );
  return (
    <Result state={state}>
      {(commit) => (
        <div className="latest-commit">
          <span className="avatar" aria-hidden="true">
            {commit.author.slice(0, 1).toUpperCase()}
          </span>
          <strong>{commit.author}</strong>
          <Link
            className="truncate"
            href={repoHref(repo, { view: "commit", rev: commit.oid })}
          >
            {commit.message.split("\n")[0]}
          </Link>
          <Link
            className="oid"
            href={repoHref(repo, { view: "commit", rev: commit.oid })}
          >
            {short(commit.oid)}
          </Link>
          <span className="muted commit-date">
            {date(commit.author_seconds)}
          </span>
          <Link className="commit-history-link" href={historyHref}>
            <HistoryIcon /> History
          </Link>
        </div>
      )}
    </Result>
  );
}
