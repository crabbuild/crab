import {
  endpoint,
  useRequest,
  type Changes,
  type Commit,
  type Repository,
} from "@/lib/api";
import { CommitList } from "@/features/repository/browse";
import { CommitSummary } from "@/features/repository/commits/summary";
import { ChangeComparison } from "@/features/repository/diff";
import { Button } from "@/components/ui/button";
import type { CodeThemes } from "@/lib/code-theme";
import { repoHref } from "@/lib/repository-route";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { GitCommitIcon } from "@primer/octicons-react";

import type { Page, PullRequest } from "./model";
import { branch } from "./presentation";

export function PullCommits({
  repo,
  pull,
  url,
  selected,
  theme,
  codeThemes,
}: {
  repo: Repository;
  pull: PullRequest;
  url: URL;
  selected?: string;
  theme: "light" | "dark";
  codeThemes: CodeThemes;
}) {
  const cursor = url.searchParams.get("commit_after") ?? undefined;
  const headRepo = pull.head_repository ?? repo;
  const saved = pull.merge
    ? `pulls/${pull.number}/merged-comparison/commits`
    : undefined;
  const commits = useRequest<Page<Commit, string>>(
    selected
      ? null
      : saved
        ? endpoint(repo, saved, { limit: "50", cursor })
        : endpoint(headRepo, "commits", {
            rev: pull.head_oid,
            base: pull.base_oid,
            upstream: pull.head_repository ? repo.id : undefined,
            limit: "50",
            cursor,
          }),
    saved ? repo.id : undefined,
  );
  function href(after?: string, oid?: string) {
    return repoHref(repo, {
      view: "pulls",
      pull: String(pull.number),
      pull_tab: "commits",
      pull_commit: oid,
      commit_after: after,
      repository_id: saved ? repo.id : undefined,
    });
  }
  if (selected) {
    return (
      <section className="pull-commits grid min-w-0 gap-4">
        <Link className="justify-self-start" href={href(cursor)}>
          Back to commits
        </Link>
        {saved ? (
          <SavedCommit
            repo={repo}
            resource={`${saved}/${encodeURIComponent(selected)}`}
            theme={theme}
            codeThemes={codeThemes}
          />
        ) : (
          <div className="notice">Select a commit from the pull request.</div>
        )}
      </section>
    );
  }
  return (
    <section className="pull-commits" aria-labelledby="pull-commits-heading">
      <div className="section-heading">
        <div>
          <h3 id="pull-commits-heading">Commits</h3>
          {saved ? (
            <p className="muted">Original commits at merge time.</p>
          ) : (
            <p className="muted">
              Commits proposed from <code>{branch(pull.head_ref)}</code> into{" "}
              <code>{branch(pull.base_ref)}</code>.
            </p>
          )}
        </div>
        <Button variant="outline" size="sm" onClick={commits.retry}>
          Refresh
        </Button>
      </div>
      <Result state={commits} timingLabel="Pull request commits">
        {(page) =>
          page.items.length ? (
            <section className="panel">
              <CommitList
                repo={headRepo}
                commits={page.items}
                commitHref={saved ? (oid) => href(cursor, oid) : undefined}
              />
            </section>
          ) : (
            <div className="notice">
              <GitCommitIcon size={24} />
              <strong>No commits to show</strong>
            </div>
          )
        }
      </Result>
      {(cursor || commits.data?.next) && (
        <div className="discussion-pagination">
          {cursor && <Link href={href()}>Newest commits</Link>}
          {commits.data?.next && (
            <Link href={href(commits.data.next)}>Older commits →</Link>
          )}
        </div>
      )}
    </section>
  );
}

function SavedCommit({
  repo,
  resource,
  theme,
  codeThemes,
}: {
  repo: Repository;
  resource: string;
  theme: "light" | "dark";
  codeThemes: CodeThemes;
}) {
  const commit = useRequest<Changes & { metadata: Commit }>(
    endpoint(repo, resource),
    repo.id,
  );
  return (
    <Result state={commit} timingLabel="Pull request commit">
      {(summary) => (
        <>
          <CommitSummary value={summary.metadata} />
          <ChangeComparison
            state={commit}
            repo={repo}
            rev={summary.commit}
            theme={theme}
            codeThemes={codeThemes}
            fileAction={`${resource}/file`}
          />
        </>
      )}
    </Result>
  );
}
