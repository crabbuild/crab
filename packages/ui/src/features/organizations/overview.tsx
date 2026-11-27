import { OrganizationIcon, RepoIcon } from "@primer/octicons-react";
import { RelativeTime } from "@/components/discussion/time";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { Badge } from "@/components/ui/badge";
import { OrganizationAvatar, UserAvatar } from "@/components/ui/user-avatar";
import { useRequest, type Repository } from "@/lib/api";
import type {
  Organization,
  OrganizationMembers,
  OrganizationRole,
} from "@/lib/contracts";
import { organizationHref } from "@/lib/organization-route";
import { repoHref } from "@/lib/repository-route";
import { InviteDialog } from "./invitations";

type Repositories = {
  repositories: (Pick<
    Repository,
    "id" | "owner" | "name" | "description" | "archived" | "visibility"
  > & { fork: boolean; metadata_updated_at_ms?: number })[];
  next: string | null;
};

export function OrganizationOverview({
  organization,
  role,
  csrf,
  onInvited,
}: {
  organization: Organization;
  role: OrganizationRole;
  csrf: string;
  onInvited: () => void;
}) {
  const endpoint = `/api/organizations/${organization.id}`;
  const repositories = useRequest<Repositories>(
    `/api/repos?${new URLSearchParams({ organization: organization.id, limit: "6" })}`,
  );
  const members = useRequest<OrganizationMembers>(`${endpoint}/members`);
  return (
    <>
      <header className="mb-6 flex min-w-0 flex-wrap items-center gap-5 border-b pb-6">
        <OrganizationAvatar
          id={organization.id}
          size={88}
          className="rounded-xl"
        />
        <div className="min-w-0 flex-1 basis-60">
          <h1
            dir="auto"
            className="m-0 text-2xl font-semibold tracking-tight wrap-anywhere"
          >
            {organization.name}
          </h1>
          {organization.description && (
            <p
              dir="auto"
              className="mt-1 mb-0 whitespace-pre-wrap wrap-anywhere text-muted-foreground"
            >
              {organization.description}
            </p>
          )}
          <p className="mt-2 mb-0 flex items-center gap-1.5 text-sm text-muted-foreground">
            <OrganizationIcon aria-hidden="true" />
            {role === "owner"
              ? "You’re an owner of this organization"
              : "You’re a member of this organization"}
          </p>
        </div>
      </header>
      <div className="grid items-start gap-8 lg:grid-cols-[minmax(0,1fr)_18.5rem]">
        <section aria-labelledby="overview-repositories" className="min-w-0">
          <div className="mb-3 flex flex-wrap items-baseline justify-between gap-2">
            <h2
              id="overview-repositories"
              className="m-0 text-base font-semibold"
            >
              Repositories
            </h2>
            <Link
              className="text-sm"
              href={organizationHref(organization, "?tab=repositories")}
            >
              View all repositories
            </Link>
          </div>
          <Result state={repositories} timingLabel="Organization repositories">
            {({ repositories }) =>
              repositories.length ? (
                <ul
                  aria-label="Recent repositories"
                  className="m-0 grid list-none gap-3 p-0 sm:grid-cols-2"
                >
                  {repositories.map((repo) => (
                    <li
                      key={repo.id}
                      className="grid min-w-0 content-start gap-1.5 rounded-lg border p-4"
                    >
                      <div className="flex min-w-0 flex-wrap items-center gap-2">
                        <RepoIcon
                          size={16}
                          aria-hidden="true"
                          className="shrink-0 text-muted-foreground"
                        />
                        <Link
                          href={repoHref(repo)}
                          className="min-w-0 font-semibold wrap-anywhere"
                        >
                          {repo.name}
                        </Link>
                        <Badge variant="outline">
                          {repo.visibility === "public" ? "Public" : "Private"}
                        </Badge>
                        {repo.archived && (
                          <Badge variant="outline">Archived</Badge>
                        )}
                      </div>
                      {repo.description && (
                        <p
                          dir="auto"
                          className="m-0 line-clamp-2 text-sm wrap-anywhere text-muted-foreground"
                        >
                          {repo.description}
                        </p>
                      )}
                      {repo.metadata_updated_at_ms !== undefined && (
                        <p className="m-0 text-xs text-muted-foreground">
                          Updated{" "}
                          <RelativeTime value={repo.metadata_updated_at_ms} />
                        </p>
                      )}
                    </li>
                  ))}
                </ul>
              ) : (
                <p
                  role="status"
                  className="m-0 rounded-lg border p-6 text-center text-sm text-muted-foreground"
                >
                  This organization has no repositories you can see yet.
                </p>
              )
            }
          </Result>
        </section>
        <aside aria-labelledby="overview-people" className="grid min-w-0 gap-3">
          <div className="flex flex-wrap items-baseline justify-between gap-2">
            <h2 id="overview-people" className="m-0 text-base font-semibold">
              People
            </h2>
            <Link
              className="text-sm"
              href={organizationHref(organization, "?tab=people")}
            >
              View all
            </Link>
          </div>
          <Result state={members} timingLabel="Organization people">
            {({ items }) => (
              <ul
                aria-label="Organization members"
                className="m-0 flex list-none flex-wrap gap-1.5 p-0"
              >
                {items.slice(0, 24).map((member) => {
                  const name = member.profile?.name ?? "Member";
                  return (
                    <li key={member.actor}>
                      {member.profile ? (
                        <Link
                          href={`/-/users/${member.actor}`}
                          aria-label={`${name} (${member.profile.handle})`}
                          title={member.profile.handle}
                          className="block rounded-full"
                        >
                          <UserAvatar
                            name={member.profile.handle}
                            avatar={member.profile.avatar}
                            size={36}
                          />
                        </Link>
                      ) : (
                        <UserAvatar
                          name={member.actor}
                          size={36}
                          title="Member without a public profile"
                        />
                      )}
                    </li>
                  );
                })}
              </ul>
            )}
          </Result>
          {role === "owner" && (
            <div>
              <InviteDialog
                endpoint={endpoint}
                csrf={csrf}
                onInvited={onInvited}
              />
            </div>
          )}
        </aside>
      </div>
    </>
  );
}
