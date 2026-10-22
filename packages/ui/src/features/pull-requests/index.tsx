import { type Refs, type Repository } from "@/lib/api";
import type { CodeThemes } from "@/lib/code-theme";
import { repoHref, repositoryRoute } from "@/lib/repository-route";
import { Link } from "@/components/navigation/link";

import { NewPull } from "./create";
import { PullDetail } from "./detail";
import { PullList } from "./list";

export function PullRequests({
  repo,
  refs,
  url,
  csrf,
  theme,
  codeThemes,
}: {
  repo: Repository;
  refs: Refs;
  url: URL;
  csrf: string;
  theme: "light" | "dark";
  codeThemes: CodeThemes;
}) {
  const pull = repositoryRoute(url)?.pull;
  if (pull === "new")
    return repo.archived ? (
      <div className="notice">
        <h2>This repository is read-only</h2>
        <p>Unarchive it before creating a pull request.</p>
        <Link href={repoHref(repo, { view: "pulls" })}>Back to pulls</Link>
      </div>
    ) : (
      <NewPull
        repo={repo}
        refs={refs}
        url={url}
        csrf={csrf}
        theme={theme}
        codeThemes={codeThemes}
      />
    );
  if (pull) {
    const number = Number(pull);
    if (!Number.isSafeInteger(number) || number <= 0)
      return (
        <div className="notice error">
          <h1>Pull request not found</h1>
          <Link href={repoHref(repo, { view: "pulls" })}>Back to pulls</Link>
        </div>
      );
    return (
      <PullDetail
        key={number}
        repo={repo}
        number={number}
        url={url}
        csrf={csrf}
        theme={theme}
        codeThemes={codeThemes}
      />
    );
  }
  return <PullList repo={repo} url={url} csrf={csrf} />;
}
