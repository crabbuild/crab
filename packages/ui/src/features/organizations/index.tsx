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
import { Field, FieldGroup, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import { FieldSelect } from "@/components/ui/field-select";
import { useRequest } from "@/lib/api";
import { MutationError } from "./feedback";
import type {
  OrganizationMembership,
  Organization,
  OrganizationMembers,
  OrganizationRole,
  OrganizationView,
  InvitationState,
  Team,
} from "@/lib/contracts";
import { Teams } from "./teams";
import type { Commands } from "./teams/commands";
import { Invitations } from "./invitations";
import { OrganizationRepositories } from "./repositories";
import { MemberAccess } from "./repository-access";
import { OrganizationAuditLog } from "./audit-log";
import { OrganizationAutomation } from "./automation";
import { OrganizationRulesets } from "@/features/settings/rulesets";
import { OrganizationRepositoryProperties } from "./repository-properties";
import { OrganizationIssueTypes } from "./issue-types";
import { organizationHref } from "./directory";
import { TeamPicker } from "@/features/settings/team-picker";
import { RepositoryPolicySettings } from "./repository-policy";
import { ProjectPolicySettings } from "./project-policy";
export { OrganizationInvitation } from "./invitations";

const adminTabs = [
  { key: "settings", label: "Settings" },
  { key: "invitations", label: "Invitations" },
  { key: "rulesets", label: "Rulesets" },
  { key: "properties", label: "Properties" },
  { key: "issue-types", label: "Issue types" },
  { key: "automation", label: "Automation" },
  { key: "audit", label: "Audit log" },
];

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
        {({ organization, role, actor, can_create }) => (
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
              ].map(({ key, label, icon: Icon }) => (
                <Button
                  key={key}
                  asChild
                  variant={tab === key ? "secondary" : "ghost"}
                >
                  <Link
                    href={organizationHref(organization, `?tab=${key}`)}
                    aria-current={tab === key ? "page" : undefined}
                  >
                    <Icon data-icon="inline-start" />
                    {label}
                  </Link>
                </Button>
              ))}
              <Button asChild variant="ghost">
                <Link href={organizationHref(organization, "/projects")}>
                  <FolderKanbanIcon data-icon="inline-start" />
                  Projects
                </Link>
              </Button>
            </nav>
            {role === "owner" && (
              <nav
                aria-label="Organization administration"
                className="flex flex-wrap items-center gap-1 border-b py-2"
              >
                <SettingsIcon
                  aria-hidden="true"
                  className="mx-2 size-4 text-muted-foreground"
                />
                {adminTabs.map(({ key, label }) => (
                  <Button
                    asChild
                    key={key}
                    size="sm"
                    variant={tab === key ? "secondary" : "ghost"}
                  >
                    <Link
                      href={organizationHref(organization, `?tab=${key}`)}
                      aria-current={tab === key ? "page" : undefined}
                    >
                      {label}
                    </Link>
                  </Button>
                ))}
              </nav>
            )}
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
                  canCreate={role === "owner" || can_create}
                  owner={role === "owner"}
                  url={url}
                  csrf={csrf}
                />
              ) : tab === "teams" ? (
                <Teams
                  id={id}
                  actor={actor}
                  owner={role === "owner"}
                  url={url}
                  csrf={csrf}
                  commands={commands}
                />
              ) : tab === "people" ? (
                <People
                  id={id}
                  endpoint={endpoint}
                  csrf={csrf}
                  owner={role === "owner"}
                  onChanged={(message) => {
                    setNotice(message);
                    own.retry();
                  }}
                />
              ) : role === "owner" && tab === "invitations" ? (
                <Invitations endpoint={endpoint} id={id} csrf={csrf} />
              ) : role === "owner" && tab === "rulesets" ? (
                <OrganizationRulesets key={id} organization={id} csrf={csrf} />
              ) : role === "owner" && tab === "properties" ? (
                <OrganizationRepositoryProperties
                  key={id}
                  organization={id}
                  csrf={csrf}
                />
              ) : role === "owner" && tab === "issue-types" ? (
                <OrganizationIssueTypes
                  key={id}
                  organization={id}
                  csrf={csrf}
                />
              ) : role === "owner" && tab === "automation" ? (
                <OrganizationAutomation organization={id} csrf={csrf} />
              ) : role === "owner" && tab === "audit" ? (
                <OrganizationAuditLog id={id} url={url} />
              ) : role === "owner" && tab === "settings" ? (
                <div className="grid gap-8">
                  <Settings
                    key={organization.version}
                    organization={organization}
                    csrf={csrf}
                    endpoint={endpoint}
                    onSaved={() => {
                      setNotice("Organization updated.");
                      own.retry();
                    }}
                  />
                  <RepositoryPolicySettings organization={id} csrf={csrf} />
                  <ProjectPolicySettings organization={id} />
                </div>
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
        )}
      </Result>
    </section>
  );
}
function People({
  id,
  endpoint,
  csrf,
  owner,
  onChanged,
}: {
  id: string;
  endpoint: string;
  csrf: string;
  owner: boolean;
  onChanged: (message: string) => void;
}) {
  const [after, setAfter] = useState<string>();
  const [query, setQuery] = useState("");
  const [q, setQ] = useState("");
  const [role, setRole] = useState("all");
  const [team, setTeam] = useState<Team>();
  const [status, setStatus] = useState<"active" | InvitationState>("active");
  const invites = owner && status !== "active";
  const params = new URLSearchParams(after ? { after } : {});
  if (q) params.set("q", q);
  if (role !== "all") params.set("role", role);
  if (team) params.set("team", team.id);
  const members = useRequest<OrganizationMembers>(
    invites ? null : `${endpoint}/members?${params}`,
  );
  const refresh = () => {
    setAfter(undefined);
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
            setAfter(undefined);
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
              setAfter(undefined);
              setRole(value);
            }}
          />
        </Field>
        {owner && (
          <Field className="w-auto">
            <FieldLabel htmlFor="people-status">Membership status</FieldLabel>
            <FieldSelect
              id="people-status"
              label="Membership status"
              value={status}
              options={{
                active: "Active members",
                pending: "Pending invitations",
                expired: "Expired invitations",
                accepted: "Accepted invitations",
                declined: "Declined invitations",
                cancelled: "Cancelled invitations",
              }}
              onValueChange={(value) => {
                setAfter(undefined);
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
                setAfter(undefined);
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
                setAfter(undefined);
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
                <Button asChild>
                  <Link href={`/-/organizations/${id}?tab=invitations`}>
                    Invite people
                  </Link>
                </Button>
              )}
            </div>
          </div>
          <Result state={members} timingLabel="Organization members">
            {(data) => (
              <>
                {data.items.length ? (
                  <ul className="divide-y rounded-lg border">
                    {data.items.map((member) => (
                      <MemberRow
                        key={`${member.actor}:${member.version}`}
                        member={member}
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
                    onClick={() => setAfter(undefined)}
                  >
                    First page
                  </Button>
                  <Button
                    variant="outline"
                    disabled={!data.next}
                    onClick={() => setAfter(data.next ?? undefined)}
                  >
                    Next page
                  </Button>
                </div>
              </>
            )}
          </Result>
        </>
      )}
    </div>
  );
}
function MemberRow({
  member,
  organization,
  canManage,
  endpoint,
  csrf,
  onChanged,
}: {
  member: OrganizationMembership;
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
function Settings({
  organization,
  endpoint,
  csrf,
  onSaved,
}: {
  organization: Organization;
  endpoint: string;
  csrf: string;
  onSaved: () => void;
}) {
  const [name, setName] = useState(organization.name);
  const [description, setDescription] = useState(organization.description);
  const mutation = useMutation(csrf, null);
  const [version, setVersion] = useState(organization.version);
  const latest = useRequest<OrganizationView>(
    mutation.conflict ? endpoint : null,
  );
  return (
    <form
      className="max-w-2xl"
      onSubmit={(event) => {
        event.preventDefault();
        void mutation
          .run(endpoint, "PATCH", {
            version,
            name,
            description,
          })
          .then((value) => {
            if (value) onSaved();
          });
      }}
    >
      <h2 className="mb-6 text-lg font-semibold">Organization profile</h2>
      <FieldGroup>
        <Field>
          <FieldLabel htmlFor="organization-name">Display name</FieldLabel>
          <Input
            id="organization-name"
            value={name}
            onChange={(event) => setName(event.target.value)}
            required
            maxLength={100}
            disabled={mutation.pending}
          />
        </Field>
        <Field>
          <FieldLabel htmlFor="organization-description">
            Description
          </FieldLabel>
          <Textarea
            id="organization-description"
            value={description}
            onChange={(event) => setDescription(event.target.value)}
            maxLength={4000}
            disabled={mutation.pending}
          />
        </Field>
        <MutationError error={mutation.error} />
        {mutation.conflict && (
          <Result state={latest} timingLabel="Current organization profile">
            {(saved) => (
              <Alert>
                <AlertDescription>
                  <p>
                    Saved profile: <strong>{saved.organization.name}</strong>
                  </p>
                  <p className="whitespace-pre-wrap">
                    {saved.organization.description || "No description"}
                  </p>
                  {saved.role === "owner" ? (
                    <div className="mt-3 flex flex-wrap gap-2">
                      <Button
                        type="button"
                        variant="outline"
                        onClick={() => {
                          setVersion(saved.organization.version);
                          mutation.reset();
                        }}
                      >
                        Keep draft with current version
                      </Button>
                      <Button
                        type="button"
                        variant="outline"
                        onClick={() => {
                          setName(saved.organization.name);
                          setDescription(saved.organization.description);
                          setVersion(saved.organization.version);
                          mutation.reset();
                        }}
                      >
                        Use saved profile
                      </Button>
                    </div>
                  ) : (
                    <p>
                      You no longer have permission to edit this organization.
                    </p>
                  )}
                </AlertDescription>
              </Alert>
            )}
          </Result>
        )}
        <Button type="submit" className="w-fit" disabled={mutation.pending}>
          {mutation.pending ? "Saving…" : "Save changes"}
        </Button>
      </FieldGroup>
    </form>
  );
}
