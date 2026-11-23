import { Link } from "@/components/navigation/link";
import type { OrganizationView, Session } from "@/lib/contracts";
import { useRequest, type Refs, type Repository } from "@/lib/api";
import { repoHref } from "@/lib/repository-route";
import { DeployTokens } from "./deploy-tokens";
import { DefaultBranchSettings } from "./general";
import { MemberSettings } from "./members";
import { MentionSettings } from "./mentions";
import { BranchProtectionSettings } from "./protections";
import { IntegrationSettings } from "./integrations";
import { RepositoryRulesets } from "./rulesets";
import { RepositoryTeamSettings } from "./team-access";
import { OutboundWebhooks } from "./webhooks";

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
  section:
    | "general"
    | "branches"
    | "members"
    | "integrations"
    | "webhooks"
    | "deploy-tokens"
    | "rulesets"
    | "mentions"
    | "team-access";
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
        <Link
          className={section === "webhooks" ? "active" : ""}
          aria-current={section === "webhooks" ? "page" : undefined}
          href={repoHref(repo, { view: "settings", section: "webhooks" })}
        >
          Webhooks
        </Link>
        {identity && (
          <Link
            className={section === "deploy-tokens" ? "active" : ""}
            aria-current={section === "deploy-tokens" ? "page" : undefined}
            href={repoHref(repo, {
              view: "settings",
              section: "deploy-tokens",
            })}
          >
            Deploy tokens
          </Link>
        )}
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
      {section === "rulesets" ? (
        <RepositoryRulesets key={repo.id} repo={repo} csrf={csrf} />
      ) : section === "integrations" ? (
        <IntegrationSettings key={repo.id} repo={repo} csrf={csrf} />
      ) : section === "webhooks" ? (
        <OutboundWebhooks key={repo.id} repo={repo} csrf={csrf} />
      ) : section === "deploy-tokens" ? (
        <DeployTokens key={repo.id} repo={repo} csrf={csrf} />
      ) : teamSection ? (
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
