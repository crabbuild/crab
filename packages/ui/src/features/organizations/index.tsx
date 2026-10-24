import { useState } from "react";
import { Building2Icon, SettingsIcon, UsersIcon } from "lucide-react";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { useMutation } from "@/components/discussion/mutations";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Field, FieldGroup, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import {
  NativeSelect,
  NativeSelectOption,
} from "@/components/ui/native-select";
import { useRequest } from "@/lib/api";
import { MutationError } from "./feedback";
import type {
  Membership,
  Organization,
  OrganizationPage as Page,
  OrganizationRole,
  OrganizationView,
} from "./model";
import { Teams } from "./teams";
import type { Commands } from "./teams/commands";
import { Invitations } from "./invitations";
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
        {({ organization, role, actor }) => (
          <>
            <header className="mb-6 flex min-w-0 items-center gap-4">
              <Building2Icon
                className="size-12 shrink-0 rounded-lg border p-2 text-muted-foreground"
                aria-hidden="true"
              />
              <div className="min-w-0 break-words">
                <h1 className="text-2xl font-semibold tracking-tight">
                  {organization.name}
                </h1>
                <p className="mt-1 text-sm text-muted-foreground">
                  {organization.description || "Organization"}
                </p>
              </div>
            </header>
            <nav
              aria-label="Organization"
              className="mb-6 flex flex-wrap gap-2 border-b pb-3"
            >
              {[
                { key: "people", label: "People", icon: UsersIcon },
                { key: "teams", label: "Teams", icon: UsersIcon },
                ...(role === "owner"
                  ? [
                      {
                        key: "invitations",
                        label: "Invitations",
                        icon: UsersIcon,
                      },
                      {
                        key: "settings",
                        label: "Settings",
                        icon: SettingsIcon,
                      },
                    ]
                  : []),
              ].map(({ key, label, icon: Icon }) => (
                <Button
                  key={key}
                  asChild
                  variant={tab === key ? "secondary" : "ghost"}
                >
                  <Link
                    href={`/-/organizations/${id}?tab=${key}`}
                    aria-current={tab === key ? "page" : undefined}
                  >
                    <Icon data-icon="inline-start" />
                    {label}
                  </Link>
                </Button>
              ))}
              <Button variant="outline" onClick={own.retry}>
                Refresh organization
              </Button>
            </nav>
            {notice && (
              <p role="status" className="mb-4 text-sm">
                {notice}
              </p>
            )}
            {tab === "teams" ? (
              <Teams
                id={id}
                actor={actor}
                owner={role === "owner"}
                url={url}
                commands={commands}
              />
            ) : tab === "people" ? (
              <People
                endpoint={endpoint}
                csrf={csrf}
                onChanged={(message) => {
                  setNotice(message);
                  own.retry();
                }}
              />
            ) : role === "owner" && tab === "invitations" ? (
              <Invitations endpoint={endpoint} id={id} csrf={csrf} />
            ) : role === "owner" && tab === "settings" ? (
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
            ) : (
              <p>Page not found.</p>
            )}
          </>
        )}
      </Result>
    </section>
  );
}
function People({
  endpoint,
  csrf,
  onChanged,
}: {
  endpoint: string;
  csrf: string;
  onChanged: (message: string) => void;
}) {
  const [after, setAfter] = useState<string>();
  const members = useRequest<Page<Membership> & { can_manage: boolean }>(
    `${endpoint}/members?${new URLSearchParams(after ? { after } : {})}`,
  );
  const refresh = () => {
    setAfter(undefined);
    members.retry();
  };
  return (
    <div className="flex flex-col gap-4">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <h2 className="text-lg font-semibold">Organization members</h2>
        <Button variant="outline" onClick={refresh}>
          Refresh members
        </Button>
      </div>
      <Result state={members} timingLabel="Organization members">
        {(data) => (
          <>
            <ul className="divide-y rounded-lg border">
              {data.items.map((member) => (
                <MemberRow
                  key={`${member.actor}:${member.version}`}
                  member={member}
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
    </div>
  );
}
function MemberRow({
  member,
  canManage,
  endpoint,
  csrf,
  onChanged,
}: {
  member: Membership;
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
              <NativeSelect
                aria-label={`Role for ${name}`}
                value={role}
                disabled={mutation.pending}
                onChange={(event) =>
                  setRole(event.target.value as OrganizationRole)
                }
              >
                <NativeSelectOption value="member">Member</NativeSelectOption>
                <NativeSelectOption value="owner">Owner</NativeSelectOption>
              </NativeSelect>
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
