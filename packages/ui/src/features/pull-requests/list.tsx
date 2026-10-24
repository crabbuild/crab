import { discussionFilters } from "@/components/discussion/list";
import { DiscussionResults } from "@/components/discussion/results";
import { navigate, type Repository } from "@/lib/api";
import { DiscussionSearch } from "@/components/discussion/search";
import { Button } from "@/components/ui/button";
import { repoHref } from "@/lib/repository-route";
import { Link } from "@/components/navigation/link";
import { TagIcon } from "lucide-react";

export function PullList({
  repo,
  url,
  csrf,
}: {
  repo: Repository;
  url: URL;
  csrf: string;
}) {
  const filters = discussionFilters(url);
  const query = url.searchParams.get("q") ?? "";
  return (
    <section className="pulls-page">
      <div className="section-heading flex-wrap">
        <h2>Pull requests</h2>
        <div className="flex flex-wrap items-center gap-2">
          <Button variant="outline" asChild>
            <Link href={repoHref(repo, { view: "labels" })}>
              <TagIcon data-icon="inline-start" />
              Labels
            </Link>
          </Button>
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
      <DiscussionResults repo={repo} url={url} view="pulls" csrf={csrf} />
    </section>
  );
}
