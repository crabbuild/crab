import { discussionFilters } from "@/components/discussion/list";
import { DiscussionResults } from "@/components/discussion/results";
import { repoHref } from "@/lib/repository-route";

import { Button } from "@/components/ui/button";

import { navigate, type Repository } from "@/lib/api";

import { DiscussionSearch } from "@/components/discussion/search";

export function IssueList({ repo, url }: { repo: Repository; url: URL }) {
  const filters = discussionFilters(url);
  const query = url.searchParams.get("q") ?? "";
  return (
    <section className="issues-page">
      <div className="issues-heading">
        <h2>All issues</h2>
        {!repo.archived && (
          <Button
            variant="default"
            onClick={() =>
              navigate(repoHref(repo, { view: "issues", issue: "new" }))
            }
          >
            New issue
          </Button>
        )}
      </div>
      <DiscussionSearch
        repo={repo}
        label="Search issues"
        placeholder="Search all issues"
        value={query}
        onSearch={(value) =>
          navigate(
            repoHref(repo, {
              view: "issues",
              ...filters,
              q: value || undefined,
            }),
          )
        }
      />
      <DiscussionResults repo={repo} url={url} view="issues" />
    </section>
  );
}
