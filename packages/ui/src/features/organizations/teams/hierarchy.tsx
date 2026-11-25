import { useId, useState } from "react";
import { ChevronDownIcon, NetworkIcon } from "lucide-react";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Field, FieldDescription, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from "@/components/ui/popover";
import type {
  Team,
  TeamCommand,
  TeamHierarchyDecision,
  TeamHierarchyRequests,
  TeamHierarchySide,
  TeamListing,
} from "@/lib/contracts";
import type { Commands } from "./commands";
import { TeamPages } from "./members";
import { teamHref } from "./model";
import { useTeamRead } from "./read";

export type TeamChoice = Pick<Team, "id" | "name" | "version"> & {
  canManage: boolean;
};

export function HierarchyPicker({
  organization,
  currentTeam,
  mode = "create",
  value,
  disabled,
  generation,
  onChange,
}: {
  organization: string;
  currentTeam?: string;
  mode?: "create" | "parent" | "child";
  value: TeamChoice | null;
  disabled: boolean;
  generation: number;
  onChange: (team: TeamChoice | null) => void;
}) {
  const id = useId();
  const [open, setOpen] = useState(false);
  const [search, setSearch] = useState("");
  const [after, setAfter] = useState<string>();
  const query = new URLSearchParams({ search: search.trim() });
  if (after) query.set("after", after);
  const state = useTeamRead<TeamListing>(
    open ? `/api/organizations/${organization}/teams?${query}` : null,
    generation,
  );
  const label = mode === "child" ? "Child team" : "Parent team";
  function select(team: TeamChoice | null) {
    onChange(team);
    setOpen(false);
  }
  return (
    <Field>
      <FieldLabel htmlFor={id}>{label}</FieldLabel>
      <Popover open={open} onOpenChange={setOpen}>
        <PopoverTrigger asChild>
          <Button
            id={id}
            type="button"
            variant="outline"
            disabled={disabled}
            className="w-fit max-w-full"
          >
            <NetworkIcon data-icon="inline-start" />
            <span className="truncate">
              {value?.name ??
                (mode === "child" ? "Choose a child team" : "No parent team")}
            </span>
            <ChevronDownIcon data-icon="inline-end" />
          </Button>
        </PopoverTrigger>
        <PopoverContent
          align="start"
          aria-label={`Choose a ${label.toLowerCase()}`}
          className="max-w-[calc(100vw-2rem)]"
        >
          <div className="grid gap-3">
            <Input
              type="search"
              aria-label={`Search ${label.toLowerCase()}s`}
              placeholder="Search teams…"
              maxLength={100}
              value={search}
              onChange={(event) => {
                setSearch(event.target.value);
                setAfter(undefined);
              }}
            />
            {mode !== "child" && (
              <Button
                type="button"
                variant="ghost"
                className="justify-start"
                onClick={() => select(null)}
              >
                No parent team
              </Button>
            )}
            <Result state={state} timingLabel={label}>
              {(data) => {
                const options = data.items.filter(
                  ({ team, can_manage }) =>
                    (mode !== "create" || can_manage) &&
                    team.visibility === "closed" &&
                    team.id !== currentTeam &&
                    (mode !== "child" || team.parent !== currentTeam),
                );
                return (
                  <>
                    {options.length ? (
                      <ul className="max-h-48 overflow-auto">
                        {options.map(({ team, can_manage }) => (
                          <li key={team.id}>
                            <Button
                              type="button"
                              variant="ghost"
                              className="w-full justify-start"
                              onClick={() =>
                                select({
                                  id: team.id,
                                  name: team.name,
                                  version: team.version,
                                  canManage: can_manage,
                                })
                              }
                            >
                              <span className="truncate">{team.name}</span>
                              {!can_manage && (
                                <span className="shrink-0 text-xs text-muted-foreground">
                                  Approval required
                                </span>
                              )}
                            </Button>
                          </li>
                        ))}
                      </ul>
                    ) : (
                      <p className="text-sm text-muted-foreground">
                        No available teams on this page.
                      </p>
                    )}
                    <TeamPages
                      after={after}
                      next={data.next}
                      onPage={setAfter}
                    />
                  </>
                );
              }}
            </Result>
          </div>
        </PopoverContent>
      </Popover>
      <FieldDescription>
        Child teams inherit their parent’s repository access.
      </FieldDescription>
    </Field>
  );
}

export function HierarchySettings({
  organization,
  actor,
  team,
  side,
  currentParent,
  commands,
}: {
  organization: string;
  actor: string;
  team: Team;
  side: TeamHierarchySide;
  currentParent?: TeamChoice | null;
  commands: Commands;
}) {
  const [draft, setDraft] = useState<TeamChoice | null>();
  const [confirmation, setConfirmation] = useState<{
    command: TeamCommand;
    description: string;
  }>();
  const choosingParent = side === "child";
  const selected = draft === undefined ? (currentParent ?? null) : draft;
  const changed = choosingParent
    ? (selected?.id ?? null) !== (team.parent ?? null)
    : selected !== null;
  function review() {
    const child = choosingParent ? team : selected;
    const parent = choosingParent ? selected : team;
    if (!child) return;
    const requiresApproval = !!selected && !selected.canManage;
    const command: TeamCommand =
      requiresApproval && parent
        ? {
            kind: "propose_hierarchy",
            team: child.id,
            parent: parent.id,
            version: child.version,
            side,
          }
        : {
            kind: "reparent",
            team: child.id,
            parent: parent?.id ?? null,
            version: child.version,
          };
    const description = parent
      ? `${child.name} and its child teams will inherit repository roles from ${parent.name}. Access inherited from the previous hierarchy may be removed.`
      : `${child.name} and its child teams will lose access granted by the current parent and its ancestors.`;
    setConfirmation({ command, description });
  }
  const pending = confirmation?.command.kind === "propose_hierarchy";
  const title = choosingParent
    ? confirmation?.command.kind === "reparent" && !confirmation.command.parent
      ? "Remove parent team"
      : "Change parent team"
    : "Add child team";
  return (
    <section
      className="grid gap-3"
      aria-label={
        choosingParent ? "Team hierarchy settings" : "Add an existing team"
      }
    >
      <HierarchyPicker
        organization={organization}
        currentTeam={team.id}
        mode={choosingParent ? "parent" : "child"}
        value={selected}
        disabled={commands.locked}
        generation={commands.generation}
        onChange={setDraft}
      />
      <Button
        className="w-fit"
        type="button"
        variant="outline"
        disabled={commands.locked || !changed}
        onClick={review}
      >
        {choosingParent ? "Review parent change" : "Review child team"}
      </Button>
      <Dialog
        open={confirmation !== undefined}
        onOpenChange={(open) => {
          if (!open && !commands.locked) setConfirmation(undefined);
        }}
      >
        <DialogContent>
          <DialogHeader>
            <DialogTitle>{pending ? `${title} request` : title}</DialogTitle>
            <DialogDescription className="wrap-anywhere">
              {confirmation?.description}
              {pending &&
                ` A ${choosingParent ? "parent" : "child"} team maintainer must approve the request before access changes.`}
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button
              type="button"
              variant="outline"
              disabled={commands.locked}
              onClick={() => setConfirmation(undefined)}
            >
              Cancel
            </Button>
            <Button
              type="button"
              disabled={!confirmation || commands.locked}
              onClick={() => {
                if (!confirmation) return;
                void commands
                  .submit(
                    organization,
                    actor,
                    confirmation.command,
                    pending ? "Send team hierarchy request" : title,
                  )
                  .then((result) => {
                    if (result?.outcome === "applied") setDraft(undefined);
                    setConfirmation(undefined);
                  });
              }}
            >
              {pending
                ? "Send request"
                : choosingParent
                  ? "Confirm parent change"
                  : "Confirm child team"}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </section>
  );
}

export function HierarchyRequests({
  organization,
  actor,
  team,
  request,
  commands,
}: {
  organization: string;
  actor: string;
  team?: string;
  request?: string | null;
  commands: Commands;
}) {
  const [after, setAfter] = useState<string>();
  const query = new URLSearchParams();
  if (after) query.set("after", after);
  if (team) query.set("team", team);
  if (request) query.set("request", request);
  const state = useTeamRead<TeamHierarchyRequests>(
    `/api/organizations/${organization}/teams/hierarchy-requests?${query}`,
    commands.generation,
  );
  const Heading = team ? "h3" : "h2";
  return (
    <section className="grid gap-4" aria-labelledby="hierarchy-requests">
      <header className="flex flex-wrap items-center justify-between gap-2">
        <Heading id="hierarchy-requests" className="text-lg font-semibold">
          Team hierarchy requests
        </Heading>
        <Button
          type="button"
          variant="outline"
          size="sm"
          onClick={() => {
            setAfter(undefined);
            state.retry();
          }}
        >
          Refresh requests
        </Button>
      </header>
      {request ? (
        <Link
          className="w-fit text-sm"
          href={teamHref(organization, team, "requests")}
        >
          All team requests
        </Link>
      ) : (
        !team && (
          <Link className="w-fit text-sm" href={teamHref(organization)}>
            All teams
          </Link>
        )
      )}
      <Result state={state} timingLabel="Team hierarchy requests">
        {({ items, next }) => (
          <>
            {items.length ? (
              <ul className="divide-y rounded-lg border">
                {items.map((item) => {
                  function decide(
                    decision: TeamHierarchyDecision,
                    label: string,
                  ) {
                    void commands.submit(
                      organization,
                      actor,
                      {
                        kind: "decide_hierarchy",
                        team: item.child,
                        request: item.id,
                        decision,
                      },
                      label,
                    );
                  }
                  return (
                    <li key={item.id} className="grid min-w-0 gap-2 p-4">
                      <p className="wrap-anywhere font-medium">
                        <Link href={teamHref(organization, item.child)}>
                          {item.child_name}
                        </Link>
                        {" under "}
                        <Link href={teamHref(organization, item.parent)}>
                          {item.parent_name}
                        </Link>
                      </p>
                      <p className="text-sm text-muted-foreground">
                        {item.can_review && !item.can_approve
                          ? "The team or requester’s permissions changed. Decline this request and ask for a new one."
                          : "Approval moves the child team and updates the repository access it inherits."}
                      </p>
                      <div className="flex flex-wrap items-center gap-2">
                        {item.can_review && (
                          <>
                            <Button
                              type="button"
                              size="sm"
                              disabled={commands.locked || !item.can_approve}
                              onClick={() =>
                                decide("approve", "Approve team request")
                              }
                            >
                              Approve
                            </Button>
                            <Button
                              type="button"
                              size="sm"
                              variant="outline"
                              disabled={commands.locked}
                              onClick={() =>
                                decide("decline", "Decline team request")
                              }
                            >
                              Decline
                            </Button>
                          </>
                        )}
                        {item.can_withdraw && (
                          <>
                            <Badge variant="outline">Awaiting approval</Badge>
                            <Button
                              type="button"
                              size="sm"
                              variant="outline"
                              disabled={commands.locked}
                              onClick={() =>
                                decide("withdraw", "Withdraw team request")
                              }
                            >
                              Withdraw request
                            </Button>
                          </>
                        )}
                      </div>
                    </li>
                  );
                })}
              </ul>
            ) : (
              <p className="text-sm text-muted-foreground">
                {request
                  ? "This team request is no longer pending."
                  : "No pending team hierarchy requests."}
              </p>
            )}
            <TeamPages after={after} next={next} onPage={setAfter} />
          </>
        )}
      </Result>
    </section>
  );
}

export function ChildTeams({
  organization,
  team,
  generation,
}: {
  organization: string;
  team: string;
  generation: number;
}) {
  const [after, setAfter] = useState<string>();
  const query = new URLSearchParams({ parent: team });
  if (after) query.set("after", after);
  const state = useTeamRead<TeamListing>(
    `/api/organizations/${organization}/teams?${query}`,
    generation,
  );
  return (
    <section className="grid gap-4" aria-labelledby="child-teams">
      <h3 id="child-teams" className="text-lg font-semibold">
        Child teams
      </h3>
      <Result state={state} timingLabel="Child teams">
        {(data) => (
          <>
            {data.items.length ? (
              <ul className="divide-y rounded-lg border">
                {data.items.map(({ team: child, members }) => (
                  <li
                    key={child.id}
                    className="flex min-w-0 flex-wrap items-center justify-between gap-2 p-4"
                  >
                    <Link
                      className="min-w-0 wrap-anywhere font-medium"
                      href={teamHref(organization, child.id)}
                    >
                      {child.name}
                    </Link>
                    <span className="text-sm text-muted-foreground">
                      {members.toLocaleString()} direct{" "}
                      {members === 1 ? "member" : "members"}
                    </span>
                  </li>
                ))}
              </ul>
            ) : (
              <p className="text-sm text-muted-foreground">
                This team has no child teams.
              </p>
            )}
            <TeamPages after={after} next={data.next} onPage={setAfter} />
          </>
        )}
      </Result>
    </section>
  );
}
