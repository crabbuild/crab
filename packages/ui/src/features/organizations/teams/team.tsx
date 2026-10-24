import { useEffect, useState } from "react";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Separator } from "@/components/ui/separator";
import { teamHref, type Draft, type View } from "./model";
import type { Commands } from "./commands";
import { TeamFields } from "./form";
import { TeamMembers } from "./members";
import { useTeamRead } from "./read";

export function TeamDetail({
  organization,
  actor,
  team,
  url,
  commands,
}: {
  organization: string;
  actor: string;
  team: string;
  url: URL;
  commands: Commands;
}) {
  const [draft, setDraft] = useState<Draft | null>(null);
  const [deleting, setDeleting] = useState(false);
  const state = useTeamRead<View>(
    `/api/organizations/${organization}/teams/${team}`,
    commands.generation,
  );
  const current = state.data;
  const command = commands.resolved?.intent.body.command;
  const applied = commands.resolved?.acknowledgement.outcome === "applied";
  useEffect(() => {
    if (!current || state.loading || state.error) return;
    setDraft((old) => {
      if (!old) return current.team;
      if (
        applied &&
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
  }, [current, state.loading, state.error, command, applied, team]);
  const settings = url.searchParams.get("view") === "settings";
  const failure = commands.failure?.body.command;
  const conflict =
    commands.conflict && failure?.kind === "edit" && failure.team === team;
  return (
    <div className="flex flex-col gap-5">
      <Link href={teamHref(organization)} className="w-fit text-sm">
        ← All teams
      </Link>
      <Result state={state} timingLabel="Team">
        {(view) => (
          <>
            <header className="flex flex-wrap items-start justify-between gap-4">
              <div className="min-w-0">
                <div className="flex flex-wrap items-center gap-3">
                  <h2 className="break-words text-2xl font-semibold tracking-tight">
                    {view.team.name}
                  </h2>
                  <Badge variant="outline">
                    {view.team.visibility === "secret" ? "Secret" : "Visible"}
                  </Badge>
                </div>
                <p className="mt-2 whitespace-pre-wrap break-words text-sm text-muted-foreground">
                  {view.team.description || "No description provided."}
                </p>
              </div>
              <Button variant="outline" onClick={state.retry}>
                Refresh team
              </Button>
            </header>
            <nav aria-label="Team" className="flex gap-2 border-b pb-3">
              <Button asChild variant={!settings ? "secondary" : "ghost"}>
                <Link
                  href={teamHref(organization, team)}
                  aria-current={!settings ? "page" : undefined}
                >
                  Members
                </Link>
              </Button>
              {view.can_manage && (
                <Button asChild variant={settings ? "secondary" : "ghost"}>
                  <Link
                    href={`${teamHref(organization, team)}&view=settings`}
                    aria-current={settings ? "page" : undefined}
                  >
                    Settings
                  </Link>
                </Button>
              )}
            </nav>
            {!settings ? (
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
                              void commands
                                .submit(
                                  organization,
                                  actor,
                                  {
                                    kind: "delete",
                                    team,
                                    version: view.team.version,
                                  },
                                  "Delete team",
                                )
                                .then((value) => {
                                  if (value) setDeleting(false);
                                })
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
