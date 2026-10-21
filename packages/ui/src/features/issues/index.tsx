import { repoHref, repositoryRoute } from "@/repository-route";

import { type Repository } from "@/api";
import { Link } from "@/ui";

import { IssueList } from "./list";
import { NewIssue } from "./create";
import { IssueDetail } from "./detail";

export function Issues({
  repo,
  url,
  csrf,
}: {
  repo: Repository;
  url: URL;
  csrf: string;
}) {
  const issue = repositoryRoute(url)?.issue;
  if (issue === "new")
    return repo.archived ? (
      <div className="notice">
        <h2>This repository is read-only</h2>
        <p>Unarchive it before creating an issue.</p>
        <Link href={repoHref(repo, { view: "issues" })}>Back to issues</Link>
      </div>
    ) : (
      <NewIssue key="new" repo={repo} csrf={csrf} />
    );
  if (issue) {
    const number = Number(issue);
    if (!Number.isSafeInteger(number) || number <= 0)
      return (
        <div className="notice error">
          <h1>Issue not found</h1>
          <Link href={repoHref(repo, { view: "issues" })}>Back to issues</Link>
        </div>
      );
    return <IssueDetail key={number} repo={repo} number={number} csrf={csrf} />;
  }
  return <IssueList repo={repo} url={url} />;
}
