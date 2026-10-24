import { Link } from "@/components/navigation/link";
import { type Refs, type Repository, type Session } from "@/lib/api";
import { repoHref } from "@/lib/repository-route";
import { DefaultBranchSettings } from "./general";
import { MemberSettings } from "./members";
import { BranchProtectionSettings } from "./protections";

export function Settings({
  repo,
  refs,
  csrf,
  section,
  identity,
  onDefaultChanged,
  onRepositoryChanged,
}: {
  repo: Repository;
  refs: Refs;
  csrf: string;
  section: "general" | "branches" | "members";
  identity: Session["user"];
  onDefaultChanged: () => void;
  onRepositoryChanged: () => void;
}) {
  return (
    <div className="settings-layout">
      <nav className="settings-nav" aria-label="Repository settings">
        <strong>Settings</strong>
        <Link
          className={section === "general" ? "active" : ""}
          aria-current={section === "general" ? "page" : undefined}
          href={repoHref(repo, { view: "settings" })}
        >
          General
        </Link>
        <Link
          className={section === "branches" ? "active" : ""}
          aria-current={section === "branches" ? "page" : undefined}
          href={repoHref(repo, { view: "settings", section: "branches" })}
        >
          Branches
        </Link>
        <Link
          className={section === "members" ? "active" : ""}
          aria-current={section === "members" ? "page" : undefined}
          href={repoHref(repo, { view: "settings", section: "members" })}
        >
          Members
        </Link>
      </nav>
      {section === "members" ? (
        <MemberSettings repo={repo} csrf={csrf} identity={identity} />
      ) : section === "branches" ? (
        <BranchProtectionSettings
          repo={repo}
          csrf={csrf}
          onRepositoryChanged={onRepositoryChanged}
        />
      ) : (
        <DefaultBranchSettings
          repo={repo}
          refs={refs}
          csrf={csrf}
          onChanged={onDefaultChanged}
          onRepositoryChanged={onRepositoryChanged}
        />
      )}
    </div>
  );
}
