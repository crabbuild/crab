import { Link } from "@/components/navigation/link";
import type { OrganizationView, Session } from "@/lib/contracts";
import { useRequest, type Refs, type Repository } from "@/lib/api";
import { repoHref } from "@/lib/repository-route";
import { DefaultBranchSettings } from "./general";
import { MemberSettings } from "./members";
import { MentionSettings } from "./mentions";
import { BranchProtectionSettings } from "./protections";
import { RepositoryTeamSettings } from "./team-access";

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
  section: "general" | "branches" | "members" | "mentions" | "team-access";
  identity: Session["user"];
  onDefaultChanged: () => void;
  onRepositoryChanged: () => void;
}) {
  const organization = identity ? repo.organization_id : undefined;
  const ownership = useRequest<OrganizationView>(
    organization
      ? `/api/organizations/${encodeURIComponent(organization)}`
      : null,
  );
  const teamSection = section === "team-access";
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
        {ownership.data?.role === "owner" && (
          <Link
            className={teamSection ? "active" : ""}
            aria-current={teamSection ? "page" : undefined}
            href={repoHref(repo, { view: "settings", section: "team-access" })}
          >
            Team access
          </Link>
        )}
        <Link
          className={section === "mentions" ? "active" : ""}
          aria-current={section === "mentions" ? "page" : undefined}
          href={repoHref(repo, { view: "settings", section: "mentions" })}
        >
          Team mentions
        </Link>
      </nav>
      {teamSection ? (
        organization ? (
          <RepositoryTeamSettings
            repo={repo}
            organization={organization}
            csrf={csrf}
          />
        ) : (
          <div className="notice error" role="alert">
            <p>Team access settings are unavailable.</p>
          </div>
        )
      ) : section === "members" ? (
        <MemberSettings repo={repo} csrf={csrf} identity={identity} />
      ) : section === "mentions" ? (
        <MentionSettings key={repo.id} repo={repo} csrf={csrf} />
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
