import { CommentVisibilityProvider } from "@/components/discussion/conversation/comment-visibility";
import { ConversationLockProvider } from "@/components/discussion/conversation/lock";
import { repoHref, repositoryRoute } from "@/lib/repository-route";

import { type Repository } from "@/lib/api";
import { Link } from "@/components/navigation/link";

import { IssueList } from "./list";
import { IssueTemplatePicker } from "./templates";
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
      <IssueTemplatePicker
        key={repo.id}
        repo={repo}
        csrf={csrf}
        selected={url.searchParams.get("template")}
      />
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
    return (
      <ConversationLockProvider
        key={`${repo.id}:${number}`}
        repo={repo}
        target={`issues/${number}`}
      >
        <CommentVisibilityProvider>
          <IssueDetail repo={repo} number={number} csrf={csrf} />
        </CommentVisibilityProvider>
      </ConversationLockProvider>
    );
  }
  return <IssueList repo={repo} url={url} csrf={csrf} />;
}
