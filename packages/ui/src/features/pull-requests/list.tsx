import { discussionFilters } from "@/components/discussion/list";
import { DiscussionResults } from "@/components/discussion/results";
import { navigate, type Repository } from "@/lib/api";
import { DiscussionSearch } from "@/components/discussion/search";
import { Button } from "@/components/ui/button";
import { repoHref } from "@/lib/repository-route";
import { Link } from "@/components/navigation/link";

export function PullList({ repo, url }: { repo: Repository; url: URL }) {
  const filters = discussionFilters(url);
  const query = url.searchParams.get("q") ?? "";
  return (
    <section className="pulls-page">
      <div className="section-heading">
        <h2>Pull requests</h2>
        {!repo.archived && (
          <Button
            variant="default"
            onClick={() =>
              navigate(repoHref(repo, { view: "pulls", pull: "new" }))
            }
          >
            New pull request
          </Button>
        )}
      </div>
      <p className="muted">Review and discuss changes between branches.</p>
      <DiscussionSearch
        repo={repo}
        label="Search pulls"
        placeholder="Search titles, descriptions, or authors"
        value={query}
        onSearch={(value) =>
          navigate(
            repoHref(repo, {
              view: "pulls",
              ...filters,
              q: value || undefined,
            }),
          )
        }
      />
      <div className="discussion-list-actions">
        <Link className="button-link" href={repoHref(repo, { view: "labels" })}>
          Labels
        </Link>
      </div>
      <DiscussionResults repo={repo} url={url} view="pulls" />
    </section>
  );
}
