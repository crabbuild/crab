import { endpoint, useRequest, type Commit, type Repository } from "@/lib/api";
import { CommitList } from "@/features/repository/browse";
import { Button } from "@/components/ui/button";
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
}: {
  repo: Repository;
  pull: PullRequest;
  url: URL;
}) {
  const cursor = url.searchParams.get("commit_after") ?? undefined;
  const commits = useRequest<Page<Commit, string>>(
    endpoint(repo, "commits", {
      rev: pull.head_oid,
      base: pull.base_oid,
      limit: "50",
      cursor,
    }),
  );
  return (
    <section className="pull-commits" aria-labelledby="pull-commits-heading">
      <div className="section-heading">
        <div>
          <h3 id="pull-commits-heading">Commits</h3>
          <p className="muted">
            Changes reachable from <code>{branch(pull.head_ref)}</code> and not
            from <code>{branch(pull.base_ref)}</code>.
          </p>
        </div>
        <Button variant="outline" size="sm" onClick={commits.retry}>
          Refresh
        </Button>
      </div>
      <Result state={commits} timingLabel="Pull request commits">
        {(page) => (
          <>
            {page.items.length ? (
              <section className="panel">
                <CommitList repo={repo} commits={page.items} />
              </section>
            ) : (
              <div className="notice">
                <GitCommitIcon size={24} />
                <strong>No commits to show</strong>
                <p>
                  The head contains no commits that are absent from the base.
                </p>
              </div>
            )}
            <div className="discussion-pagination">
              {cursor && (
                <Link
                  href={repoHref(repo, {
                    view: "pulls",
                    pull: String(pull.number),
                    pull_tab: "commits",
                  })}
                >
                  Newest commits
                </Link>
              )}
              {page.next && (
                <Link
                  href={repoHref(repo, {
                    view: "pulls",
                    pull: String(pull.number),
                    pull_tab: "commits",
                    commit_after: page.next,
                  })}
                >
                  Older commits →
                </Link>
              )}
            </div>
          </>
        )}
      </Result>
    </section>
  );
}
