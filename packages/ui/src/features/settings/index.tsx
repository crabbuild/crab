import { Link } from "@/components/navigation/link";
import type { Session } from "@/lib/contracts";
import { type Refs, type Repository } from "@/lib/api";
import { repoHref } from "@/lib/repository-route";
import { DefaultBranchSettings } from "./general";
import { MemberSettings } from "./members";
import { BranchProtectionSettings } from "./protections";
import { IntegrationSettings } from "./integrations";
import { RepositoryRulesets } from "./rulesets";

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
  section: "general" | "branches" | "members" | "integrations" | "rulesets";
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
          className={section === "rulesets" ? "active" : ""}
          aria-current={section === "rulesets" ? "page" : undefined}
          href={repoHref(repo, { view: "settings", section: "rulesets" })}
        >
          Rulesets
        </Link>
        <Link
          className={section === "members" ? "active" : ""}
          aria-current={section === "members" ? "page" : undefined}
          href={repoHref(repo, { view: "settings", section: "members" })}
        >
          Members
        </Link>
        <Link
          className={section === "integrations" ? "active" : ""}
          aria-current={section === "integrations" ? "page" : undefined}
          href={repoHref(repo, { view: "settings", section: "integrations" })}
        >
          Integrations
        </Link>
      </nav>
      {section === "rulesets" ? (
        <RepositoryRulesets key={repo.id} repo={repo} csrf={csrf} />
      ) : section === "integrations" ? (
        <IntegrationSettings key={repo.id} repo={repo} csrf={csrf} />
      ) : section === "members" ? (
        repo.forked_from ? (
          <div className="notice">
            This fork inherits access from the source repository.{" "}
            <Link
              href={repoHref(repo.forked_from, {
                view: "settings",
                section: "members",
              })}
            >
              Manage source access
            </Link>
          </div>
        ) : (
          <MemberSettings repo={repo} csrf={csrf} identity={identity} />
        )
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
