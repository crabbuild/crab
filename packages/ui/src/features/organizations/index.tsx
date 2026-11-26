import { useState } from "react";
import {
  GearIcon,
  HomeIcon,
  PeopleIcon,
  PersonIcon,
  RepoIcon,
  TableIcon,
} from "@primer/octicons-react";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import type { Loaded } from "@/lib/api";
import type { OrganizationView } from "@/lib/contracts";
import { organizationHref } from "@/lib/organization-route";
import { OrganizationSetup } from "./new";
import { OrganizationOverview } from "./overview";
import { OrganizationPeople } from "./people";
import { OrganizationRepositories } from "./repositories";
import { OrganizationSettings, isOrganizationSettings } from "./settings";
import { Teams } from "./teams";
import type { Commands } from "./teams/commands";
export { OrganizationInvitation } from "./invitations";

export function OrganizationPage({
  id,
  own,
  url,
  csrf,
  commands,
}: {
  id: string;
  own: Loaded<OrganizationView> & { retry: () => void };
  url: URL;
  csrf: string;
  commands: Commands;
}) {
  const tab = url.searchParams.get("tab") ?? "overview";
  const [notice, setNotice] = useState("");
  return (
    <Result state={own} timingLabel="Organization">
      {({ organization, role, actor, can_create }) => {
        const owner = role === "owner";
        const endpoint = `/api/organizations/${id}`;
        if (owner && url.searchParams.get("setup") === "members")
          return (
            <OrganizationSetup
              organization={organization}
              endpoint={endpoint}
              csrf={csrf}
            />
          );
        const settings = isOrganizationSettings(tab);
        const peopleView =
          tab === "people" ||
          (owner && (tab === "invitations" || tab === "outside-collaborators"))
            ? tab
            : undefined;
        const current = settings ? "settings" : peopleView ? "people" : tab;
        const tabs = [
          { key: "overview", label: "Overview", Icon: HomeIcon, href: "" },
          { key: "repositories", label: "Repositories", Icon: RepoIcon },
          {
            key: "projects",
            label: "Projects",
            Icon: TableIcon,
            href: "/projects",
          },
          { key: "teams", label: "Teams", Icon: PeopleIcon },
          { key: "people", label: "People", Icon: PersonIcon },
          ...(owner
            ? [{ key: "settings", label: "Settings", Icon: GearIcon }]
            : []),
        ];
        return (
          <>
            <div className="repo-header">
              <nav aria-label="Organization">
                {tabs.map(({ key, label, Icon, href }) => (
                  <Link
                    key={key}
                    className={current === key ? "active" : ""}
                    aria-current={current === key ? "page" : undefined}
                    href={organizationHref(organization, href ?? `?tab=${key}`)}
                  >
                    <Icon
                      aria-hidden="true"
                      className="text-muted-foreground"
                    />
                    {label}
                  </Link>
                ))}
              </nav>
            </div>
            <div className="mx-auto w-full max-w-[1280px] px-(--page-gutter) py-(--page-block)">
              {tab !== "overview" && (
                <h1 dir="auto" className="sr-only">
                  {organization.name}
                </h1>
              )}
              {notice && (
                <p role="status" className="mb-4 text-sm">
                  {notice}
                </p>
              )}
              {tab === "overview" ? (
                <OrganizationOverview
                  organization={organization}
                  role={role}
                  csrf={csrf}
                  onInvited={() =>
                    setNotice(
                      "Invitation created. It appears in the invitee’s notifications.",
                    )
                  }
                />
              ) : tab === "repositories" ? (
                <OrganizationRepositories
                  id={id}
                  name={organization.name}
                  actor={actor}
                  canCreate={owner || can_create}
                  owner={owner}
                  url={url}
                  csrf={csrf}
                />
              ) : tab === "teams" ? (
                <Teams
                  id={id}
                  actor={actor}
                  owner={owner}
                  url={url}
                  csrf={csrf}
                  commands={commands}
                />
              ) : peopleView ? (
                <OrganizationPeople
                  organization={organization}
                  actor={actor}
                  owner={owner}
                  view={peopleView}
                  url={url}
                  csrf={csrf}
                  onNotice={setNotice}
                  onChanged={(message) => {
                    setNotice(message);
                    own.retry();
                  }}
                />
              ) : owner && settings ? (
                <OrganizationSettings
                  tab={tab}
                  organization={organization}
                  endpoint={endpoint}
                  csrf={csrf}
                  url={url}
                  onProfileSaved={() => {
                    setNotice("Organization updated.");
                    own.retry();
                  }}
                />
              ) : (
                <div className="notice error" role="alert">
                  <p>This organization page is unavailable.</p>
                  <Link href={organizationHref(organization)}>
                    Back to organization
                  </Link>
                </div>
              )}
            </div>
          </>
        );
      }}
    </Result>
  );
}
