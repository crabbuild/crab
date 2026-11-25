import { useEffect, useState } from "react";
import { ArrowLeftIcon } from "lucide-react";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Separator } from "@/components/ui/separator";
import { BrowseRepositories } from "@/features/repository/catalog-browse";
import { ProjectLibrary } from "@/features/projects/workspace/library";
import { navigate } from "@/lib/api";
import type { TeamView } from "@/lib/contracts";
import { teamHref, type Draft } from "./model";
import type { Commands } from "./commands";
import { TeamFields } from "./form";
import { TeamMembers } from "./members";
import { IdentityGroupSettings } from "./identity-group";
import { useTeamRead } from "./read";
import { TeamReviewRouting } from "./review-routing";
import { RepositoryGrant } from "./repository-grant";
import { ChildTeams, HierarchyRequests, HierarchySettings } from "./hierarchy";

export function TeamDetail({
  organization,
  actor,
  team,
  url,
  csrf,
  owner,
  commands,
}: {
  organization: string;
  actor: string;
  team: string;
  url: URL;
  csrf: string;
  owner: boolean;
  commands: Commands;
}) {
  const [draft, setDraft] = useState<Draft | null>(null);
  const [addingChild, setAddingChild] = useState(false);
  const [deleting, setDeleting] = useState<{
    version: number;
    hierarchy_version?: number;
  }>();
  const [grantSaved, setGrantSaved] = useState<{
    name: string;
    count: number;
  }>();
  const state = useTeamRead<TeamView>(
    `/api/organizations/${organization}/teams/${team}`,
    commands.generation,
  );
  const current = state.data;
  const parentId = current?.team.parent;
  const parent = useTeamRead<TeamView>(
    parentId ? `/api/organizations/${organization}/teams/${parentId}` : null,
    commands.generation,
  );
  const resolution = commands.resolved;
  const command =
    resolution?.intent.actor === actor &&
    resolution.intent.organization === organization &&
    resolution.acknowledgement.outcome === "applied"
      ? resolution.intent.body.command
      : undefined;
  const deleted = command?.kind === "delete" && command.team === team;
  useEffect(() => {
    if (deleted) navigate(teamHref(organization));
  }, [deleted, organization]);
  useEffect(() => {
    if (!current || state.loading || state.error) return;
    setDraft((old) => {
      if (!old) return current.team;
      if (
        command?.kind === "edit" &&
        command.team === team &&
        old.version === command.version &&
        current.team.name === command.name &&
        current.team.description === command.description &&
        current.team.visibility === command.visibility
      )
        return { ...old, version: current.team.version };
      return old;
    });
  }, [current, state.loading, state.error, command, team]);
  const tab = url.searchParams.get("view");
  const settings = tab === "settings";
  const repositories = tab === "repositories";
  const projects = tab === "projects";
  const children = tab === "children";
  const requests = tab === "requests";
  const members =
    !settings && !repositories && !projects && !children && !requests;
  const failure = commands.failure?.body.command;
  const conflict =
    commands.conflict && failure?.kind === "edit" && failure.team === team;
  return (
    <div className="flex flex-col gap-5">
      <Link
        href={teamHref(organization)}
        className="flex w-fit items-center gap-2 text-sm"
      >
        <ArrowLeftIcon className="size-4" aria-hidden="true" /> All teams
      </Link>
      <Result state={state} timingLabel="Team">
        {(view) => (
          <>
            <header className="flex flex-wrap items-start justify-between gap-4">
              <div className="min-w-0">
                <div className="flex flex-wrap items-center gap-3">
                  <h2
                    dir="auto"
                    className="min-w-0 wrap-anywhere text-2xl font-semibold tracking-tight"
                  >
                    {view.team.name}
                  </h2>
                  <Badge variant="outline">
                    {view.team.visibility === "secret" ? "Secret" : "Visible"}
                  </Badge>
                  {view.role && (
                    <Badge variant="secondary">
                      {view.role === "maintainer" ? "Maintainer" : "Member"}
                    </Badge>
                  )}
                </div>
                {view.team.description && (
                  <p
                    dir="auto"
                    className="mt-2 whitespace-pre-wrap wrap-anywhere text-sm text-muted-foreground"
                  >
                    {view.team.description}
                  </p>
                )}
                {view.team.parent && (
                  <p className="mt-2 text-sm text-muted-foreground">
                    Parent team{" "}
                    <Link
                      href={teamHref(organization, view.team.parent)}
                      className="wrap-anywhere font-medium"
                    >
                      {parent.data?.team.name ?? "View parent team"}
                    </Link>
                  </p>
                )}
              </div>
              <Button variant="outline" onClick={state.retry}>
                Refresh team
              </Button>
            </header>
            <nav
              aria-label="Team"
              className="flex flex-wrap gap-2 border-b pb-3"
            >
              <Button asChild variant={members ? "secondary" : "ghost"}>
                <Link
                  href={teamHref(organization, team)}
                  aria-current={members ? "page" : undefined}
                >
                  Members
                </Link>
              </Button>
              <Button asChild variant={repositories ? "secondary" : "ghost"}>
                <Link
                  href={teamHref(organization, team, "repositories")}
                  aria-current={repositories ? "page" : undefined}
                >
                  Repositories
                </Link>
              </Button>
              <Button asChild variant={projects ? "secondary" : "ghost"}>
                <Link
                  href={teamHref(organization, team, "projects")}
                  aria-current={projects ? "page" : undefined}
                >
                  Projects
                </Link>
              </Button>
              <Button asChild variant={children ? "secondary" : "ghost"}>
                <Link
                  href={teamHref(organization, team, "children")}
                  aria-current={children ? "page" : undefined}
                >
                  Child teams
                </Link>
              </Button>
              {view.can_manage && (
                <Button asChild variant={requests ? "secondary" : "ghost"}>
                  <Link
                    href={teamHref(organization, team, "requests")}
                    aria-current={requests ? "page" : undefined}
                  >
                    Requests
                  </Link>
                </Button>
              )}
              {view.can_manage && (
                <Button asChild variant={settings ? "secondary" : "ghost"}>
                  <Link
                    href={teamHref(organization, team, "settings")}
                    aria-current={settings ? "page" : undefined}
                  >
                    Settings
                  </Link>
                </Button>
              )}
            </nav>
            {repositories ? (
              <section
                className="grid min-w-0 gap-4"
                aria-labelledby="team-repositories"
              >
                <div className="flex flex-wrap items-center justify-between gap-3">
                  <h3 id="team-repositories" className="text-lg font-semibold">
                    Team repositories
                  </h3>
                  {owner && (
                    <RepositoryGrant
                      organization={organization}
                      team={team}
                      csrf={csrf}
                      onSaved={(name) =>
                        setGrantSaved((previous) => ({
                          name,
                          count: (previous?.count ?? 0) + 1,
                        }))
                      }
                    />
                  )}
                </div>
                {grantSaved && (
                  <p role="status">Team access saved for {grantSaved.name}.</p>
                )}
                <BrowseRepositories
                  key={grantSaved?.count}
                  url={url}
                  scope={{ kind: "team", organization, team }}
                />
              </section>
            ) : projects ? (
              <ProjectLibrary
                organization={organization}
                url={url}
                scope={{ target: { team }, canLink: view.can_manage }}
              />
            ) : requests ? (
              <HierarchyRequests
                organization={organization}
                actor={actor}
                team={team}
                commands={commands}
              />
            ) : children ? (
              <div className="grid gap-4">
                {view.can_manage && view.team.visibility === "closed" && (
                  <>
                    <Button
                      type="button"
                      className="w-fit"
                      variant="outline"
                      aria-expanded={addingChild}
                      onClick={() => setAddingChild(!addingChild)}
                    >
                      {addingChild
                        ? "Close child team picker"
                        : "Add a child team"}
                    </Button>
                    {addingChild && (
                      <HierarchySettings
                        organization={organization}
                        actor={actor}
                        team={view.team}
                        side="parent"
                        commands={commands}
                      />
                    )}
                  </>
                )}
                <ChildTeams
                  organization={organization}
                  team={team}
                  generation={commands.generation}
                />
              </div>
            ) : members ? (
              <TeamMembers
                organization={organization}
                actor={actor}
                team={team}
                commands={commands}
              />
            ) : view.can_manage && draft ? (
              <div className="flex max-w-2xl flex-col gap-7">
                <form
                  className="flex flex-col gap-5"
                  onSubmit={(event) => {
                    event.preventDefault();
                    // Read records also carry IDs and timestamps. Keep the command
                    // limited to editable fields for the strict native DTO.
                    const { name, description, visibility, version } = draft;
                    void commands.submit(
                      organization,
                      actor,
                      {
                        kind: "edit",
                        team,
                        name,
                        description,
                        visibility,
                        version,
                      },
                      "Update team",
                    );
                  }}
                >
                  <h3 className="text-lg font-semibold">Team settings</h3>
                  <TeamFields
                    draft={draft}
                    onChange={setDraft}
                    disabled={commands.locked}
                  />
                  {conflict && (
                    <Alert>
                      <AlertDescription>
                        <p>
                          Your draft is preserved. Review the current saved team
                          before continuing.
                        </p>
                        <p>
                          <strong>{view.team.name}</strong> ·{" "}
                          {view.team.visibility === "secret"
                            ? "Secret"
                            : "Visible"}
                        </p>
                        <p className="whitespace-pre-wrap break-words">
                          {view.team.description || "No description"}
                        </p>
                        <div className="mt-3 flex flex-wrap gap-2">
                          <Button
                            type="button"
                            variant="outline"
                            onClick={() =>
                              setDraft({ ...draft, version: view.team.version })
                            }
                          >
                            Continue with my draft
                          </Button>
                          <Button
                            type="button"
                            variant="outline"
                            onClick={() => setDraft(view.team)}
                          >
                            Use saved team
                          </Button>
                        </div>
                      </AlertDescription>
                    </Alert>
                  )}
                  <Button
                    type="submit"
                    className="w-fit"
                    disabled={
                      commands.locked ||
                      (conflict && draft.version !== view.team.version)
                    }
                  >
                    Save team changes
                  </Button>
                </form>
                {view.team.visibility === "closed" && (
                  <>
                    <Separator />
                    <HierarchySettings
                      key={view.team.parent ?? "root"}
                      organization={organization}
                      actor={actor}
                      team={view.team}
                      side="child"
                      currentParent={
                        view.team.parent
                          ? {
                              id: view.team.parent,
                              name:
                                parent.data?.team.name ?? "Current parent team",
                              version: parent.data?.team.version ?? 0,
                              canManage: parent.data?.can_manage ?? false,
                            }
                          : null
                      }
                      commands={commands}
                    />
                  </>
                )}
                {owner && (
                  <>
                    <Separator />
                    <IdentityGroupSettings
                      path={`/api/organizations/${organization}/teams/${team}/identity-group`}
                      csrf={csrf}
                      onChanged={state.retry}
                    />
                  </>
                )}
                <Separator />
                <TeamReviewRouting
                  organization={organization}
                  actor={actor}
                  team={team}
                  commands={commands}
                />
                {(owner || view.tree_version === undefined) && (
                  <>
                    <Separator />
                    <section className="flex flex-col items-start gap-3">
                      <h3 className="font-semibold">Delete this team</h3>
                      <p className="text-sm text-muted-foreground">
                        {view.tree_version === undefined
                          ? "Deleting this team removes its memberships and repository grants."
                          : "Deleting this team also deletes all of its child teams and their grants."}
                      </p>
                      {!deleting ? (
                        <Button
                          variant="destructive"
                          disabled={commands.locked}
                          onClick={() =>
                            setDeleting({
                              version: view.team.version,
                              hierarchy_version: view.tree_version,
                            })
                          }
                        >
                          Delete team
                        </Button>
                      ) : (
                        <Alert variant="destructive">
                          <AlertDescription>
                            <p>
                              Delete {view.team.name}
                              {deleting.hierarchy_version === undefined
                                ? ""
                                : " and all of its child teams"}
                              ? This cannot be undone.
                            </p>
                            <div className="mt-3 flex flex-wrap gap-2">
                              <Button
                                variant="destructive"
                                disabled={commands.locked}
                                onClick={() =>
                                  void commands.submit(
                                    organization,
                                    actor,
                                    {
                                      kind: "delete",
                                      team,
                                      ...deleting,
                                    },
                                    "Delete team",
                                  )
                                }
                              >
                                Confirm delete team
                              </Button>
                              <Button
                                variant="outline"
                                disabled={commands.locked}
                                onClick={() => setDeleting(undefined)}
                              >
                                Keep team
                              </Button>
                            </div>
                          </AlertDescription>
                        </Alert>
                      )}
                    </section>
                  </>
                )}
              </div>
            ) : (
              <p className="text-sm text-muted-foreground">
                Only team maintainers and organization owners can manage this
                team.
              </p>
            )}
          </>
        )}
      </Result>
    </div>
  );
}
