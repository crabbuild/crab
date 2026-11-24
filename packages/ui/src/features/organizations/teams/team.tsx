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
  const [deleting, setDeleting] = useState(false);
  const [grantSaved, setGrantSaved] = useState<{
    name: string;
    count: number;
  }>();
  const state = useTeamRead<TeamView>(
    `/api/organizations/${organization}/teams/${team}`,
    commands.generation,
  );
  const current = state.data;
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
  const members = !settings && !repositories && !projects;
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
                team={{ id: team, canManage: view.can_manage }}
              />
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
                <Separator />
                <section className="flex flex-col items-start gap-3">
                  <h3 className="font-semibold">Delete this team</h3>
                  <p className="text-sm text-muted-foreground">
                    Deleting a team removes its memberships. A new team with the
                    same name will start with new memberships.
                  </p>
                  {!deleting ? (
                    <Button
                      variant="destructive"
                      disabled={commands.locked}
                      onClick={() => setDeleting(true)}
                    >
                      Delete team
                    </Button>
                  ) : (
                    <Alert variant="destructive">
                      <AlertDescription>
                        <p>Delete {view.team.name}? This cannot be undone.</p>
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
                                  version: view.team.version,
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
                            onClick={() => setDeleting(false)}
                          >
                            Keep team
                          </Button>
                        </div>
                      </AlertDescription>
                    </Alert>
                  )}
                </section>
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
