import type { ReactNode } from "react";
import { IssueOpenedIcon, TagIcon } from "@primer/octicons-react";
import { repoHref, type Repository } from "./api";
import { Link } from "./ui";

export function IssuesWorkspace({
  repo,
  view,
  showNavigation,
  children,
}: {
  repo: Repository;
  view: "issues" | "labels";
  showNavigation: boolean;
  children: ReactNode;
}) {
  if (!showNavigation) return children;
  return (
    <div className="issues-workspace">
      <aside className="issues-sidebar" aria-label="Issue navigation">
        <nav>
          <Link
            className={view === "issues" ? "active" : ""}
            aria-current={view === "issues" ? "page" : undefined}
            href={repoHref(repo, { view: "issues" })}
          >
            <IssueOpenedIcon />
            Issues
          </Link>
          <Link
            className={view === "labels" ? "active" : ""}
            aria-current={view === "labels" ? "page" : undefined}
            href={repoHref(repo, { view: "labels" })}
          >
            <TagIcon />
            Labels
          </Link>
        </nav>
      </aside>
      <div className="issues-workspace-main">{children}</div>
    </div>
  );
}
