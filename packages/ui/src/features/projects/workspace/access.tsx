import { useEffect, useRef, useState } from "react";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { FieldSelect } from "@/components/ui/field-select";
import { Input } from "@/components/ui/input";
import { Result } from "@/components/request/result";
import { useRequest } from "@/lib/api";
import type {
  ActorSummary,
  OrganizationMembers,
  TeamListing,
} from "@/lib/contracts";
import { short } from "@/lib/format";
import { CommandError, useProjectCommands } from "../commands";
import { projectEndpoint, type Project } from "../model";
import { projectAccessRoles } from "./model";
import type {
  AccessRole,
  AccessTarget,
  GrantTarget,
  ProjectAccessView,
} from "./model";

const grantRoles = { read: "Read", write: "Write", admin: "Admin" };

export function ProjectAccess({
  organization,
  project,
  onClose,
}: {
  organization: string;
  project: Project;
  onClose: () => void;
}) {
  const commands = useProjectCommands();
  const resource = useRequest<ProjectAccessView>(
    projectEndpoint({ organization }, `${project.id}/access`),
  );
  const generation = useRef(commands.generation);
  useEffect(() => {
    if (generation.current !== commands.generation) {
      generation.current = commands.generation;
      resource.retry();
    }
  }, [commands.generation, resource.retry]);
  async function save(
    version: number,
    target: AccessTarget,
    role: AccessRole | null,
    defaultVersion?: number,
  ) {
    const outcome = await commands.submitOrganization(organization, {
      kind: "set_access",
      project: project.id,
      version,
      target,
      role,
      ...(role === null ? { default_version: defaultVersion } : {}),
    });
    if (outcome?.outcome === "rejected") resource.retry();
  }
  return (
    <Dialog
      open
      onOpenChange={(open) => {
        if (!open) onClose();
      }}
    >
      <DialogContent className="max-h-[85vh] overflow-y-auto sm:max-w-xl">
        <DialogHeader>
          <DialogTitle>Manage Project access</DialogTitle>
          <DialogDescription>
            Choose who can view, edit, or administer {project.definition.name}.
            Linked work follows its repository permissions.
          </DialogDescription>
        </DialogHeader>
        <CommandError />
        <Result state={resource}>
          {(data) => (
            <div className="grid gap-5">
              <BaseRole
                key={`base-${data.version}-${data.default_version}`}
                access={data}
                disabled={commands.locked}
                onSave={(role) =>
                  void save(
                    data.version,
                    { kind: "base" },
                    role,
                    data.default_version,
                  )
                }
              />
              <AddGrant
                organization={organization}
                project={project.id}
                canManageTeams={data.can_manage_teams}
                disabled={commands.locked}
                onSave={(target, role) => void save(data.version, target, role)}
              />
              <section className="grid gap-2" aria-label="Project grants">
                <h3 className="text-sm font-medium">Collaborators</h3>
                {data.grants.length === 0 ? (
                  <p className="text-sm text-muted-foreground">
                    No explicit grants.
                  </p>
                ) : (
                  <ul className="divide-y rounded-md border">
                    {data.grants.map(({ target, role, active, label }) => {
                      const name =
                        label ??
                        `${target.kind === "member" ? "Member" : target.kind === "outside" ? "Outside collaborator" : "Team"} ${short(target.id)}`;
                      return (
                        <li
                          key={`${target.kind}-${target.id}`}
                          className="flex flex-wrap items-center gap-2 p-2"
                        >
                          <span className="min-w-28 flex-1 break-words text-sm">
                            {name}
                            {target.kind === "outside" && label && (
                              <span className="text-muted-foreground">
                                {" "}
                                · Outside collaborator
                              </span>
                            )}
                            {!active && (
                              <span className="text-muted-foreground">
                                {" "}
                                · Inactive
                              </span>
                            )}
                          </span>
                          <FieldSelect
                            label={`Role for ${name}`}
                            options={grantRoles}
                            value={role}
                            disabled={!active || commands.locked}
                            onValueChange={(value) =>
                              void save(
                                data.version,
                                target,
                                value as AccessRole,
                              )
                            }
                          />
                          <Button
                            type="button"
                            size="sm"
                            variant="ghost"
                            disabled={commands.locked}
                            aria-label={`Remove ${name} grant`}
                            onClick={() =>
                              void save(data.version, target, "none")
                            }
                          >
                            Remove
                          </Button>
                        </li>
                      );
                    })}
                  </ul>
                )}
              </section>
            </div>
          )}
        </Result>
      </DialogContent>
    </Dialog>
  );
}

function BaseRole({
  access,
  disabled,
  onSave,
}: {
  access: ProjectAccessView;
  disabled: boolean;
  onSave: (role: AccessRole | null) => void;
}) {
  const [role, setRole] = useState(access.base_override);
  return (
    <form
      className="grid gap-2 border-b pb-4"
      onSubmit={(event) => {
        event.preventDefault();
        if (role !== access.base_override) onSave(role);
      }}
    >
      <h3 className="text-sm font-medium">Access for organization members</h3>
      <div className="flex flex-wrap items-center gap-2">
        <FieldSelect
          label="Base role"
          options={{
            inherit: `Organization default (${projectAccessRoles[access.default_role]})`,
            ...projectAccessRoles,
          }}
          value={role ?? "inherit"}
          disabled={disabled}
          onValueChange={(value) =>
            setRole(value === "inherit" ? null : (value as AccessRole))
          }
        />
        <Button
          type="submit"
          size="sm"
          variant="outline"
          disabled={disabled || role === access.base_override}
        >
          Save base role
        </Button>
      </div>
    </form>
  );
}

function AddGrant({
  organization,
  project,
  canManageTeams,
  disabled,
  onSave,
}: {
  organization: string;
  project: string;
  canManageTeams: boolean;
  disabled: boolean;
  onSave: (target: GrantTarget, role: AccessRole) => void;
}) {
  const [kind, setKind] = useState<GrantTarget["kind"]>("member");
  const [input, setInput] = useState("");
  const [search, setSearch] = useState("");
  const [after, setAfter] = useState("");
  const [id, setId] = useState("");
  const [role, setRole] = useState<AccessRole>("write");
  const root = `/api/organizations/${organization}`;
  const params = new URLSearchParams(after ? { after } : {});
  const viaDirectory =
    kind === "outside" || (kind === "member" && !canManageTeams);
  if (search) params.set(kind === "team" ? "search" : "q", search);
  if (viaDirectory && kind === "member") params.set("member", "true");
  const members = useRequest<OrganizationMembers>(
    kind === "member" && !viaDirectory ? `${root}/members?${params}` : null,
  );
  const teams = useRequest<TeamListing>(
    kind === "team" ? `${root}/teams?${params}` : null,
  );
  const people = useRequest<{ items: ActorSummary[]; truncated: boolean }>(
    viaDirectory && search
      ? projectEndpoint(
          { organization },
          `${project}/collaborators`,
          Object.fromEntries(params),
        )
      : null,
  );
  const state = viaDirectory ? people : kind === "member" ? members : teams;
  const next = viaDirectory
    ? undefined
    : kind === "member"
      ? members.data?.next
      : teams.data?.next;
  const choices = viaDirectory
    ? people.data?.items.map((profile) => [
        profile.id,
        `${profile.name} (${profile.handle})`,
      ])
    : kind === "member"
      ? members.data?.items.map(({ actor, profile }) => [
          actor,
          profile
            ? `${profile.name} (${profile.handle})`
            : `Member ${short(actor)}`,
        ])
      : teams.data?.items.map(({ team }) => [team.id, team.name]);
  function searchFor(value: string) {
    setAfter("");
    setId("");
    setSearch(value);
  }
  return (
    <section
      className="grid gap-2 border-b pb-4"
      aria-label="Grant Project access"
    >
      <h3 className="text-sm font-medium">Grant access</h3>
      <form
        className="flex flex-wrap items-center gap-2"
        onSubmit={(event) => {
          event.preventDefault();
          searchFor(input.trim());
        }}
      >
        <FieldSelect
          label="Collaborator type"
          options={{
            member: "Member",
            ...(canManageTeams ? { team: "Team" } : {}),
            outside: "Outside collaborator",
          }}
          value={kind}
          disabled={disabled}
          onValueChange={(value) => {
            setKind(value as GrantTarget["kind"]);
            setInput("");
            searchFor("");
          }}
        />
        <Input
          type="search"
          aria-label="Find collaborator"
          placeholder={viaDirectory ? "Search by username" : undefined}
          value={input}
          maxLength={100}
          className="min-w-28 flex-1"
          disabled={disabled}
          onChange={(event) => setInput(event.target.value)}
        />
        <Button type="submit" size="sm" variant="outline" disabled={disabled}>
          Search
        </Button>
      </form>
      {viaDirectory && people.data?.truncated && (
        <p className="text-sm text-muted-foreground">
          Enter more of the username to narrow the search.
        </p>
      )}
      {state.error ? (
        <div className="flex flex-wrap items-center gap-2 text-sm">
          <p role="alert">{state.error}</p>
          <Button
            size="sm"
            variant="outline"
            onClick={() => {
              setAfter("");
              state.retry();
            }}
          >
            Reload choices
          </Button>
        </div>
      ) : (
        <form
          className="flex flex-wrap items-center gap-2"
          onSubmit={(event) => {
            event.preventDefault();
            if (id && choices?.some(([key]) => key === id))
              onSave({ kind, id }, role);
          }}
        >
          <FieldSelect
            label="Collaborator"
            options={Object.fromEntries([
              [
                "",
                state.loading
                  ? "Loading…"
                  : viaDirectory && !search
                    ? "Enter a username"
                    : choices?.length
                      ? "Choose collaborator"
                      : "No matches",
              ],
              ...(choices ?? []),
            ])}
            value={id}
            onValueChange={setId}
            disabled={disabled || !choices?.length}
          />
          <FieldSelect
            label="Grant role"
            options={grantRoles}
            value={role}
            onValueChange={(value) => setRole(value as AccessRole)}
            disabled={disabled}
          />
          <Button type="submit" size="sm" disabled={disabled || !id}>
            Grant access
          </Button>
          {next && (
            <Button
              type="button"
              variant="ghost"
              size="sm"
              disabled={disabled}
              onClick={() => {
                setId("");
                setAfter(next);
              }}
            >
              More choices
            </Button>
          )}
        </form>
      )}
    </section>
  );
}
