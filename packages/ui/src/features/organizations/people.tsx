import { useRef, useState, type RefObject } from "react";
import {
  KebabHorizontalIcon,
  MailIcon,
  PeopleIcon,
  PersonIcon,
  SearchIcon,
  XIcon,
} from "@primer/octicons-react";
import { useMutation } from "@/components/discussion/mutations";
import { Link } from "@/components/navigation/link";
import { SettingsNav } from "@/components/navigation/settings-nav";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import {
  Dialog,
  DialogClose,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import {
  Field,
  FieldContent,
  FieldDescription,
  FieldLabel,
} from "@/components/ui/field";
import { FieldSelect } from "@/components/ui/field-select";
import { Input } from "@/components/ui/input";
import { RadioGroup, RadioGroupItem } from "@/components/ui/radio-group";
import { UserAvatar } from "@/components/ui/user-avatar";
import { TeamPicker } from "@/features/settings/team-picker";
import { navigate, useRequest } from "@/lib/api";
import type {
  InvitationState,
  Organization,
  OrganizationMembers,
  OrganizationMembership,
  OrganizationRole,
  Team,
} from "@/lib/contracts";
import { organizationHref } from "@/lib/organization-route";
import { MutationError } from "./feedback";
import { Invitations, InviteDialog } from "./invitations";
import {
  MemberActions,
  MemberChangeDialog,
  type MemberChange,
} from "./member-actions";
import { OutsideCollaborators } from "./outside-collaborators";
import { MemberAccess } from "./repository-access";

export type PeopleView = "people" | "outside-collaborators" | "invitations";

const roles: Record<OrganizationRole, [string, string]> = {
  member: [
    "Member",
    "Can see other members and visible teams, and works in the repositories they can access.",
  ],
  owner: [
    "Owner",
    "Has full administrative rights to the organization, its teams, and all of its repositories.",
  ],
};

export function OrganizationPeople({
  organization,
  actor,
  owner,
  view,
  url,
  csrf,
  onNotice,
  onChanged,
}: {
  organization: Organization;
  actor: string;
  owner: boolean;
  view: PeopleView;
  url: URL;
  csrf: string;
  onNotice: (message: string) => void;
  onChanged: (message: string) => void;
}) {
  const endpoint = `/api/organizations/${organization.id}`;
  const content =
    view === "outside-collaborators" ? (
      <OutsideCollaborators
        endpoint={endpoint}
        actor={actor}
        url={url}
        csrf={csrf}
        onInvited={() => onNotice("Invitation sent to the collaborator.")}
      />
    ) : (
      <People
        key={view}
        id={organization.id}
        actor={actor}
        endpoint={endpoint}
        csrf={csrf}
        owner={owner}
        invites={view === "invitations"}
        onInvited={() =>
          onNotice(
            "Invitation created. It appears in the invitee’s notifications.",
          )
        }
        onChanged={onChanged}
      />
    );
  // Members see only the roster; outside collaborators and invitations are
  // owner views, as on GitHub.
  if (!owner) return content;
  return (
    <div className="account-settings-body">
      <SettingsNav
        label="Organization people"
        active={view}
        groups={[
          {
            items: [
              {
                id: "people",
                label: "Members",
                href: organizationHref(organization, "?tab=people"),
                Icon: PersonIcon,
              },
              {
                id: "outside-collaborators",
                label: "Outside collaborators",
                href: organizationHref(
                  organization,
                  "?tab=outside-collaborators",
                ),
                Icon: PeopleIcon,
              },
              {
                id: "invitations",
                label: "Invitations",
                href: organizationHref(organization, "?tab=invitations"),
                Icon: MailIcon,
              },
            ],
          },
        ]}
      />
      <div className="min-w-0">{content}</div>
    </div>
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
  const [sent, setSent] = useState(0);
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
  const finished = (
    departed: OrganizationMembership[],
    action: MemberChange,
  ) => {
    // Leaving the organization removes this page's authority.
    if (departed.some((member) => member.actor === actor)) {
      navigate("/");
      return;
    }
    const count = departed.length;
    onChanged(
      action === "removal"
        ? `Removed ${count} ${count === 1 ? "member" : "members"}.`
        : count === 1
          ? "Converted 1 member to an outside collaborator."
          : `Converted ${count} members to outside collaborators.`,
    );
    refresh();
  };
  const filters = (
    <>
      <FieldSelect
        label={invites ? "Invitation role" : "Filter by role"}
        value={role}
        options={{ all: "All roles", owner: "Owners", member: "Members" }}
        onValueChange={(value) => {
          changePage();
          setRole(value);
        }}
      />
      {invites ? (
        <FieldSelect
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
            setStatus(value as InvitationState);
          }}
        />
      ) : team ? (
        <Button
          variant="outline"
          onClick={() => {
            changePage();
            setTeam(undefined);
          }}
        >
          <span className="max-w-40 truncate">Team: {team.name}</span>
          <XIcon aria-hidden="true" />
          <span className="sr-only">Clear team filter</span>
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
      )}
    </>
  );
  return (
    <section className="flex min-w-0 flex-col gap-4">
      <div className="flex flex-wrap items-center gap-3 border-b pb-4">
        <h2 className="mr-auto text-xl font-semibold">
          {invites ? "Invitations" : "Members"}
        </h2>
        {owner && (
          <InviteDialog
            endpoint={endpoint}
            csrf={csrf}
            onInvited={() => {
              setSent((count) => count + 1);
              onInvited();
            }}
          />
        )}
      </div>
      <div className="flex flex-wrap items-center gap-2">
        <form
          role="search"
          aria-label="People search"
          className="relative min-w-0 flex-1 basis-60"
          onSubmit={(event) => {
            event.preventDefault();
            changePage();
            setQ(query.trim());
          }}
        >
          <SearchIcon
            size={16}
            aria-hidden="true"
            className="pointer-events-none absolute top-1/2 left-2.5 -translate-y-1/2 text-muted-foreground"
          />
          <Input
            type="search"
            aria-label={invites ? "Find an invitation" : "Find a member"}
            value={query}
            maxLength={100}
            placeholder={invites ? "Find an invitation…" : "Find a member…"}
            className="pl-8"
            onChange={(event) => setQuery(event.target.value)}
          />
        </form>
        {filters}
      </div>
      {invites ? (
        <Invitations
          key={JSON.stringify([q, role, status, sent])}
          endpoint={endpoint}
          id={id}
          csrf={csrf}
          filters={{ q, role, status }}
        />
      ) : (
        <Result state={members} timingLabel="Organization members">
          {(data) => {
            const selected = data.items.filter(
              (member) => checks[member.actor] === member.version,
            );
            return (
              <>
                <div className="overflow-hidden rounded-lg border">
                  <div className="flex min-h-12 flex-wrap items-center gap-3 border-b bg-muted/50 px-4 py-2">
                    {data.can_manage && data.items.length > 0 && (
                      <Checkbox
                        aria-label="Select page"
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
                    )}
                    {selected.length > 0 ? (
                      <div className="mr-auto">
                        <MemberActions
                          endpoint={endpoint}
                          csrf={csrf}
                          members={selected}
                          onReload={refresh}
                          onFinished={(action) => finished(selected, action)}
                        />
                      </div>
                    ) : (
                      <span className="mr-auto text-sm font-semibold">
                        {data.items.length}{" "}
                        {data.items.length === 1 ? "member" : "members"}
                        {(after || data.next) && " on this page"}
                      </span>
                    )}
                  </div>
                  {data.items.length ? (
                    <ul className="divide-y" aria-label="Organization members">
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
                          onRoleChanged={() => {
                            onChanged("Member role updated.");
                            refresh();
                          }}
                          onDeparted={(action) => finished([member], action)}
                          onReload={refresh}
                        />
                      ))}
                    </ul>
                  ) : (
                    <p
                      role="status"
                      className="m-0 p-8 text-center text-sm text-muted-foreground"
                    >
                      {q || role !== "all" || team
                        ? "No members match these filters."
                        : "No members on this page."}
                    </p>
                  )}
                </div>
                {(after || data.next) && (
                  <div className="flex justify-center gap-2">
                    <Button
                      variant="outline"
                      size="sm"
                      disabled={!after}
                      onClick={() => changePage()}
                    >
                      First page
                    </Button>
                    <Button
                      variant="outline"
                      size="sm"
                      disabled={!data.next}
                      onClick={() => changePage(data.next ?? undefined)}
                    >
                      Next page
                    </Button>
                  </div>
                )}
              </>
            );
          }}
        </Result>
      )}
    </section>
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
  onRoleChanged,
  onDeparted,
  onReload,
}: {
  member: OrganizationMembership;
  checked: boolean;
  onSelect: (checked: boolean) => void;
  organization: string;
  canManage: boolean;
  endpoint: string;
  csrf: string;
  onRoleChanged: () => void;
  onDeparted: (action: MemberChange) => void;
  onReload: () => void;
}) {
  const menu = useRef<HTMLButtonElement>(null);
  const [dialog, setDialog] = useState<"role" | "access" | MemberChange>();
  const name = member.profile?.name || "Member without a public profile";
  const close = () => setDialog(undefined);
  return (
    <li className="flex min-w-0 items-center gap-3 px-4 py-3">
      {canManage && (
        <Checkbox
          aria-label={`Select ${member.profile?.name ?? member.actor}`}
          checked={checked}
          onCheckedChange={(checked) => onSelect(checked === true)}
        />
      )}
      <UserAvatar
        name={member.profile?.handle || member.actor}
        avatar={member.profile?.avatar}
        size={36}
      />
      <div className="min-w-0 flex-1">
        {member.profile ? (
          <Link
            href={`/-/users/${member.actor}`}
            className="font-semibold wrap-anywhere text-foreground"
          >
            {name}
          </Link>
        ) : (
          <strong className="font-semibold wrap-anywhere">{name}</strong>
        )}
        <p className="m-0 text-sm break-all text-muted-foreground">
          {member.profile ? member.profile.handle : member.actor}
        </p>
      </div>
      <span className="shrink-0 text-sm text-muted-foreground">
        {roles[member.role][0]}
      </span>
      {canManage && (
        <>
          <DropdownMenu>
            <DropdownMenuTrigger asChild>
              <Button
                ref={menu}
                variant="ghost"
                size="icon-sm"
                aria-label={`Actions for ${name}`}
              >
                <KebabHorizontalIcon aria-hidden="true" />
              </Button>
            </DropdownMenuTrigger>
            <DropdownMenuContent
              align="end"
              className="w-max max-w-[calc(100vw-2rem)]"
              onCloseAutoFocus={(event) => {
                if (dialog) event.preventDefault();
              }}
            >
              <DropdownMenuItem onSelect={() => setDialog("role")}>
                Change role…
              </DropdownMenuItem>
              <DropdownMenuItem onSelect={() => setDialog("access")}>
                View repository access
              </DropdownMenuItem>
              <DropdownMenuSeparator />
              <DropdownMenuItem onSelect={() => setDialog("conversion")}>
                Convert to outside collaborator…
              </DropdownMenuItem>
              <DropdownMenuItem
                variant="destructive"
                onSelect={() => setDialog("removal")}
              >
                Remove from organization…
              </DropdownMenuItem>
            </DropdownMenuContent>
          </DropdownMenu>
          <ChangeRole
            member={member}
            name={name}
            endpoint={endpoint}
            csrf={csrf}
            open={dialog === "role"}
            returnFocus={menu}
            onClose={close}
            onChanged={onRoleChanged}
          />
          <MemberAccess
            organization={organization}
            actor={member.actor}
            name={member.profile?.name ?? member.actor}
            open={dialog === "access"}
            onOpenChange={(open) => {
              if (!open) close();
            }}
            returnFocus={menu}
          />
          <MemberChangeDialog
            endpoint={endpoint}
            csrf={csrf}
            members={[member]}
            action={
              dialog === "removal" || dialog === "conversion"
                ? dialog
                : undefined
            }
            returnFocus={menu}
            onClose={close}
            onFinished={onDeparted}
            onReload={onReload}
          />
        </>
      )}
    </li>
  );
}

function ChangeRole({
  member,
  name,
  endpoint,
  csrf,
  open,
  returnFocus,
  onClose,
  onChanged,
}: {
  member: OrganizationMembership;
  name: string;
  endpoint: string;
  csrf: string;
  open: boolean;
  returnFocus: RefObject<HTMLButtonElement | null>;
  onClose: () => void;
  onChanged: () => void;
}) {
  const mutation = useMutation(csrf, null);
  const [role, setRole] = useState<OrganizationRole>(member.role);
  const prefix = `role-${member.actor}`;
  async function save() {
    const value = await mutation.run(
      `${endpoint}/members/${member.actor}`,
      "PUT",
      { version: member.version, role },
    );
    if (value) {
      onClose();
      onChanged();
    }
  }
  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        if (next || mutation.pending) return;
        mutation.reset();
        setRole(member.role);
        onClose();
      }}
    >
      <DialogContent
        onCloseAutoFocus={(event) => {
          if (returnFocus.current) {
            event.preventDefault();
            returnFocus.current.focus();
          }
        }}
      >
        <DialogHeader>
          <DialogTitle className="wrap-anywhere">
            Change the role of {name}?
          </DialogTitle>
          <DialogDescription>
            Choose this member’s role in the organization.
          </DialogDescription>
        </DialogHeader>
        <RadioGroup
          aria-label="Organization role"
          value={role}
          disabled={mutation.pending}
          onValueChange={(value) => setRole(value as OrganizationRole)}
        >
          {(Object.keys(roles) as OrganizationRole[]).map((value) => {
            const [label, description] = roles[value];
            return (
              <Field key={value} orientation="horizontal">
                <RadioGroupItem
                  id={`${prefix}-${value}`}
                  value={value}
                  aria-describedby={`${prefix}-${value}-help`}
                />
                <FieldContent>
                  <FieldLabel htmlFor={`${prefix}-${value}`}>
                    {label}
                  </FieldLabel>
                  <FieldDescription id={`${prefix}-${value}-help`}>
                    {description}
                  </FieldDescription>
                </FieldContent>
              </Field>
            );
          })}
        </RadioGroup>
        <MutationError error={mutation.error} />
        <DialogFooter>
          <DialogClose asChild>
            <Button variant="outline" disabled={mutation.pending}>
              Cancel
            </Button>
          </DialogClose>
          <Button
            disabled={mutation.pending || role === member.role}
            onClick={() => void save()}
          >
            {mutation.pending ? "Changing role…" : "Change role"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
