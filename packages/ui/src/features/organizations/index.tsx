import { useState } from "react";
import {
  Building2Icon,
  SettingsIcon,
  UsersIcon,
  FolderKanbanIcon,
  BookMarkedIcon,
  NetworkIcon,
} from "lucide-react";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { useMutation } from "@/components/discussion/mutations";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Label } from "@/components/ui/label";
import { Field, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { FieldSelect } from "@/components/ui/field-select";
import { navigate, useRequest } from "@/lib/api";
import { MutationError } from "./feedback";
import type {
  OrganizationMembership,
  OrganizationMembers,
  OrganizationRole,
  OrganizationView,
  InvitationState,
  Team,
} from "@/lib/contracts";
import { Teams } from "./teams";
import type { Commands } from "./teams/commands";
import { Invitations, InviteDialog } from "./invitations";
import { OrganizationSettings, isOrganizationSettings } from "./settings";
import { OrganizationRepositories } from "./repositories";
import { MemberAccess } from "./repository-access";
import { ConvertMembers } from "./convert-members";
import { OutsideCollaborators } from "./outside-collaborators";
import { organizationHref } from "@/lib/organization-route";
import { TeamPicker } from "@/features/settings/team-picker";
export { OrganizationInvitation } from "./invitations";

export function OrganizationPage({
  id,
  url,
  csrf,
  commands,
}: {
  id: string;
  url: URL;
  csrf: string;
  commands: Commands;
}) {
  const endpoint = `/api/organizations/${id}`;
  const own = useRequest<OrganizationView>(endpoint);
  const tab = url.searchParams.get("tab") ?? "people";
  const [notice, setNotice] = useState("");
  return (
    <section className="mx-auto w-full max-w-6xl p-4 sm:p-8">
      <Result state={own} timingLabel="Organization">
        {({ organization, role, actor, can_create }) => {
          const settings = isOrganizationSettings(tab);
          const owner = role === "owner";
          const people =
            tab === "people" ||
            (owner &&
              (tab === "invitations" || tab === "outside-collaborators"));
          return (
            <>
              <header className="mb-6 flex min-w-0 flex-wrap items-start gap-4">
                <Building2Icon
                  className="size-12 shrink-0 rounded-lg border p-2 text-muted-foreground"
                  aria-hidden="true"
                />
                <div className="min-w-0 flex-1 basis-48">
                  <div className="flex flex-wrap items-center gap-2">
                    <h1
                      dir="auto"
                      className="min-w-0 wrap-anywhere text-2xl font-semibold tracking-tight"
                    >
                      {organization.name}
                    </h1>
                    <Badge variant="outline">
                      {role === "owner"
                        ? "Organization owner"
                        : "Organization member"}
                    </Badge>
                  </div>
                  {organization.description && (
                    <p
                      dir="auto"
                      className="mt-1 whitespace-pre-wrap wrap-anywhere text-sm text-muted-foreground"
                    >
                      {organization.description}
                    </p>
                  )}
                </div>
                <Button variant="outline" size="sm" onClick={own.retry}>
                  Refresh organization
                </Button>
              </header>
              <nav
                aria-label="Organization"
                className="flex flex-wrap gap-2 border-b pb-3"
              >
                {[
                  {
                    key: "repositories",
                    label: "Repositories",
                    icon: BookMarkedIcon,
                  },
                  { key: "people", label: "People", icon: UsersIcon },
                  { key: "teams", label: "Teams", icon: NetworkIcon },
                ].map(({ key, label, icon: Icon }) => {
                  const selected = key === "people" ? people : tab === key;
                  return (
                    <Button
                      key={key}
                      asChild
                      variant={selected ? "secondary" : "ghost"}
                    >
                      <Link
                        href={organizationHref(organization, `?tab=${key}`)}
                        aria-current={selected ? "page" : undefined}
                      >
                        <Icon data-icon="inline-start" />
                        {label}
                      </Link>
                    </Button>
                  );
                })}
                <Button asChild variant="ghost">
                  <Link href={organizationHref(organization, "/projects")}>
                    <FolderKanbanIcon data-icon="inline-start" />
                    Projects
                  </Link>
                </Button>
                {owner && (
                  <Button asChild variant={settings ? "secondary" : "ghost"}>
                    <Link
                      href={organizationHref(organization, "?tab=settings")}
                      aria-current={settings ? "page" : undefined}
                    >
                      <SettingsIcon data-icon="inline-start" />
                      Settings
                    </Link>
                  </Button>
                )}
              </nav>
              <div className="mt-6">
                {notice && (
                  <p role="status" className="mb-4 text-sm">
                    {notice}
                  </p>
                )}
                {tab === "repositories" ? (
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
                ) : people ? (
                  <div className="grid gap-4">
                    {owner && (
                      <nav
                        aria-label="Organization people"
                        className="flex flex-wrap gap-2"
                      >
                        {[
                          ["people", "Members"],
                          ["outside-collaborators", "Outside collaborators"],
                          ["invitations", "Invitations"],
                        ].map(([view, label]) => (
                          <Button
                            key={view}
                            asChild
                            variant={tab === view ? "secondary" : "ghost"}
                            size="sm"
                          >
                            <Link
                              href={organizationHref(
                                organization,
                                `?tab=${view}`,
                              )}
                              aria-current={tab === view ? "page" : undefined}
                            >
                              {label}
                            </Link>
                          </Button>
                        ))}
                      </nav>
                    )}
                    {tab === "outside-collaborators" ? (
                      <OutsideCollaborators
                        endpoint={endpoint}
                        actor={actor}
                        url={url}
                        csrf={csrf}
                        onInvited={() =>
                          setNotice("Invitation sent to the collaborator.")
                        }
                      />
                    ) : (
                      <People
                        key={tab}
                        id={id}
                        actor={actor}
                        endpoint={endpoint}
                        csrf={csrf}
                        owner={owner}
                        invites={tab === "invitations"}
                        onInvited={() =>
                          setNotice(
                            "Invitation created. It appears in the invitee’s notifications.",
                          )
                        }
                        onChanged={(message) => {
                          setNotice(message);
                          own.retry();
                        }}
                      />
                    )}
                  </div>
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
                    <Link href={`/-/organizations/${id}`}>
                      Back to organization
                    </Link>
                  </div>
                )}
              </div>
            </>
          );
        }}
      </Result>
    </section>
  );
}
function People({
  id,
  actor,
  endpoint,
  csrf,
  owner,
  invites,
  onInvited,
  onChanged,
}: {
  id: string;
  actor: string;
  endpoint: string;
  csrf: string;
  owner: boolean;
  invites: boolean;
  onInvited: () => void;
  onChanged: (message: string) => void;
}) {
  const [after, setAfter] = useState<string>();
  const [checks, setChecks] = useState<Record<string, number>>({});
  const changePage = (cursor?: string) => {
    setChecks({});
    setAfter(cursor);
  };
  const select = (member: OrganizationMembership, checked: boolean) => {
    setChecks((previous) => {
      const next = { ...previous };
      if (checked) next[member.actor] = member.version;
      else delete next[member.actor];
      return next;
    });
  };
  const [query, setQuery] = useState("");
  const [q, setQ] = useState("");
  const [role, setRole] = useState("all");
  const [team, setTeam] = useState<Team>();
  const [status, setStatus] = useState<InvitationState>("pending");
  const params = new URLSearchParams(after ? { after } : {});
  if (q) params.set("q", q);
  if (role !== "all") params.set("role", role);
  if (team) params.set("team", team.id);
  const members = useRequest<OrganizationMembers>(
    invites ? null : `${endpoint}/members?${params}`,
  );
  const refresh = () => {
    changePage();
    members.retry();
  };
  return (
    <div className="flex flex-col gap-4">
      <div className="flex flex-wrap items-end gap-3">
        <form
          role="search"
          aria-label="People search"
          className="flex items-end gap-2"
          onSubmit={(event) => {
            event.preventDefault();
            changePage();
            setQ(query.trim());
          }}
        >
          <Field className="min-w-0">
            <FieldLabel htmlFor="people-search">Find people</FieldLabel>
            <Input
              id="people-search"
              type="search"
              value={query}
              maxLength={100}
              placeholder="Name or username"
              onChange={(event) => setQuery(event.target.value)}
            />
          </Field>
          <Button type="submit" variant="outline">
            Search
          </Button>
        </form>
        <Field className="w-auto">
          <FieldLabel htmlFor="people-role">Role</FieldLabel>
          <FieldSelect
            id="people-role"
            label="Organization role"
            value={role}
            options={{ all: "All roles", member: "Member", owner: "Owner" }}
            onValueChange={(value) => {
              changePage();
              setRole(value);
            }}
          />
        </Field>
        {invites && (
          <Field className="w-auto">
            <FieldLabel htmlFor="people-status">Invitation status</FieldLabel>
            <FieldSelect
              id="people-status"
              label="Invitation status"
              value={status}
              options={{
                pending: "Pending invitations",
                expired: "Expired invitations",
                accepted: "Accepted invitations",
                declined: "Declined invitations",
                cancelled: "Cancelled invitations",
              }}
              onValueChange={(value) => {
                changePage();
                setStatus(value as typeof status);
              }}
            />
          </Field>
        )}
        {!invites &&
          (team ? (
            <Button
              variant="outline"
              onClick={() => {
                changePage();
                setTeam(undefined);
              }}
            >
              Clear team filter · {team.name}
            </Button>
          ) : (
            <TeamPicker
              organization={id}
              filter
              selected={new Set()}
              disabled={false}
              onSelect={(value) => {
                changePage();
                setTeam(value);
              }}
            />
          ))}
      </div>
      {invites ? (
        <Invitations
          key={JSON.stringify([q, role, status])}
          endpoint={endpoint}
          id={id}
          csrf={csrf}
          filters={{ q, role, status }}
        />
      ) : (
        <>
          <div className="flex flex-wrap items-center justify-between gap-3">
            <h2 className="text-lg font-semibold">Organization members</h2>
            <div className="flex flex-wrap gap-2">
              <Button variant="outline" onClick={refresh}>
                Refresh members
              </Button>
              {owner && (
                <InviteDialog
                  endpoint={endpoint}
                  csrf={csrf}
                  onInvited={onInvited}
                />
              )}
            </div>
          </div>
          <Result state={members} timingLabel="Organization members">
            {(data) => {
              const selected = data.items.filter(
                (member) => checks[member.actor] === member.version,
              );
              return (
                <>
                  {data.can_manage && data.items.length > 0 && (
                    <div className="flex flex-wrap items-center justify-between gap-3">
                      <Label
                        className="flex items-center gap-2"
                        htmlFor="select-members-page"
                      >
                        <Checkbox
                          id="select-members-page"
                          checked={
                            selected.length === data.items.length ||
                            (selected.length > 0 && "indeterminate")
                          }
                          onCheckedChange={(checked) =>
                            setChecks(
                              checked
                                ? Object.fromEntries(
                                    data.items.map(({ actor, version }) => [
                                      actor,
                                      version,
                                    ]),
                                  )
                                : {},
                            )
                          }
                        />
                        Select page
                      </Label>
                      {selected.length > 0 && (
                        <div className="flex flex-wrap items-center gap-2">
                          <span className="text-sm text-muted-foreground">
                            {selected.length} selected
                          </span>
                          <ConvertMembers
                            endpoint={`${endpoint}/members/conversion`}
                            csrf={csrf}
                            members={selected}
                            onReload={refresh}
                            onConverted={() => {
                              if (
                                selected.some(
                                  (member) => member.actor === actor,
                                )
                              ) {
                                navigate("/");
                              } else {
                                onChanged(
                                  selected.length === 1
                                    ? "Converted 1 member to an outside collaborator."
                                    : `Converted ${selected.length} members to outside collaborators.`,
                                );
                                refresh();
                              }
                            }}
                          />
                        </div>
                      )}
                    </div>
                  )}
                  {data.items.length ? (
                    <ul className="divide-y rounded-lg border">
                      {data.items.map((member) => (
                        <MemberRow
                          key={`${member.actor}:${member.version}`}
                          member={member}
                          checked={checks[member.actor] === member.version}
                          onSelect={(checked) => select(member, checked)}
                          organization={id}
                          canManage={data.can_manage}
                          endpoint={endpoint}
                          csrf={csrf}
                          onChanged={(message) => {
                            onChanged(message);
                            refresh();
                          }}
                        />
                      ))}
                    </ul>
                  ) : (
                    <p
                      role="status"
                      className="rounded-lg border p-6 text-center text-sm text-muted-foreground"
                    >
                      No members on this page.
                    </p>
                  )}
                  <div className="flex gap-2">
                    <Button
                      variant="outline"
                      disabled={!after}
                      onClick={() => changePage()}
                    >
                      First page
                    </Button>
                    <Button
                      variant="outline"
                      disabled={!data.next}
                      onClick={() => changePage(data.next ?? undefined)}
                    >
                      Next page
                    </Button>
                  </div>
                </>
              );
            }}
          </Result>
        </>
      )}
    </div>
  );
}
function MemberRow({
  member,
  checked,
  onSelect,
  organization,
  canManage,
  endpoint,
  csrf,
  onChanged,
}: {
  member: OrganizationMembership;
  checked: boolean;
  onSelect: (checked: boolean) => void;
  organization: string;
  canManage: boolean;
  endpoint: string;
  csrf: string;
  onChanged: (message: string) => void;
}) {
  const mutation = useMutation(csrf, null);
  const [role, setRole] = useState<OrganizationRole>(member.role);
  const [removing, setRemoving] = useState(false);
  async function save(remove: boolean) {
    const value = await mutation.run(
      `${endpoint}/members/${member.actor}`,
      remove ? "DELETE" : "PUT",
      remove ? { version: member.version } : { version: member.version, role },
    );
    if (value) onChanged(remove ? "Member removed." : "Member role updated.");
  }
  const name = member.profile?.name || "Member without a public profile";
  return (
    <li className="flex flex-col gap-3 p-4">
      <div className="flex min-w-0 flex-wrap items-center justify-between gap-4">
        <div className="flex min-w-0 items-center gap-3">
          {canManage && (
            <Checkbox
              aria-label={`Select ${member.profile?.name ?? member.actor}`}
              checked={checked}
              disabled={mutation.pending || removing}
              onCheckedChange={(checked) => onSelect(checked === true)}
            />
          )}
          <div className="min-w-0 break-words">
            {member.profile ? (
              <Link href={`/-/users/${member.actor}`} className="font-medium">
                {name}
              </Link>
            ) : (
              <strong className="font-medium">{name}</strong>
            )}
            <p className="mt-1 break-all text-sm text-muted-foreground">
              {member.profile ? `@${member.profile.handle}` : member.actor}
            </p>
          </div>
        </div>
        <div className="flex flex-wrap items-center gap-2">
          {canManage ? (
            <>
              <MemberAccess
                organization={organization}
                actor={member.actor}
                name={member.profile?.name ?? member.actor}
              />
              <FieldSelect
                label={`Role for ${name}`}
                value={role}
                disabled={mutation.pending}
                onValueChange={(role) => setRole(role as OrganizationRole)}
                options={{ member: "Member", owner: "Owner" }}
              />
              <Button
                variant="outline"
                disabled={mutation.pending || role === member.role}
                onClick={() => void save(false)}
              >
                Save role
              </Button>
              <Button
                variant="ghost"
                disabled={mutation.pending}
                onClick={() => setRemoving(true)}
              >
                Remove
              </Button>
            </>
          ) : (
            <Badge variant="outline">{member.role}</Badge>
          )}
        </div>
      </div>
      {removing && (
        <Alert>
          <AlertDescription>
            <p>Remove {name} from this organization?</p>
            <div className="mt-3 flex gap-2">
              <Button
                variant="destructive"
                disabled={mutation.pending}
                onClick={() => void save(true)}
              >
                Remove member
              </Button>
              <Button
                variant="outline"
                disabled={mutation.pending}
                onClick={() => setRemoving(false)}
              >
                Cancel
              </Button>
            </div>
          </AlertDescription>
        </Alert>
      )}
      <MutationError error={mutation.error} />
    </li>
  );
}
