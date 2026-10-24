import { useState } from "react";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  NativeSelect,
  NativeSelectOption,
} from "@/components/ui/native-select";
import { useTeamRead } from "./read";
import type { Commands } from "./commands";
import type { Member, Role, Roster } from "./model";
export function TeamMembers({
  organization,
  actor,
  team,
  commands,
}: {
  organization: string;
  actor: string;
  team: string;
  commands: Commands;
}) {
  const [adding, setAdding] = useState(false);
  const [after, setAfter] = useState<string>();
  const root = `/api/organizations/${organization}/teams/${team}`;
  const params = new URLSearchParams(after ? { after } : {});
  const members = useTeamRead<Roster>(
    `${root}/members?${params}`,
    commands.generation,
  );
  return (
    <div className="flex flex-col gap-4">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <h3 className="text-lg font-semibold">Team members</h3>
        <div className="flex gap-2">
          <Button
            variant="outline"
            onClick={() => {
              setAfter(undefined);
              members.retry();
            }}
          >
            Refresh members
          </Button>
          {members.data?.can_manage && (
            <Button
              disabled={commands.locked}
              onClick={() => setAdding((value) => !value)}
            >
              {adding ? "Close member picker" : "Add a member"}
            </Button>
          )}
        </div>
      </div>
      {adding && members.data?.can_manage && (
        <Candidates
          organization={organization}
          actor={actor}
          team={team}
          commands={commands}
        />
      )}
      <Result state={members} timingLabel="Team members">
        {(data) => (
          <>
            <ul className="divide-y rounded-lg border">
              {data.items.length ? (
                data.items.map((member) => (
                  <MemberRow
                    key={`${member.actor}:${member.version}`}
                    organization={organization}
                    actor={actor}
                    team={team}
                    member={member}
                    manage={data.can_manage}
                    commands={commands}
                  />
                ))
              ) : (
                <li className="p-8 text-center text-sm text-muted-foreground">
                  This team has no members.
                </li>
              )}
            </ul>
            <TeamPages after={after} next={data.next} onPage={setAfter} />
          </>
        )}
      </Result>
    </div>
  );
}
function Candidates({
  organization,
  actor,
  team,
  commands,
}: {
  organization: string;
  actor: string;
  team: string;
  commands: Commands;
}) {
  const [after, setAfter] = useState<string>();
  const params = new URLSearchParams(after ? { after } : {});
  const state = useTeamRead<Roster>(
    `/api/organizations/${organization}/teams/${team}/candidates?${params}`,
    commands.generation,
  );
  return (
    <section
      className="flex flex-col gap-4 rounded-lg border bg-muted/20 p-4"
      aria-label="Add organization members"
    >
      <div>
        <h4 className="font-semibold">Add organization members</h4>
        <p className="mt-1 text-sm text-muted-foreground">
          People must join the organization before they can join a team.
        </p>
      </div>
      <Button
        className="w-fit"
        variant="outline"
        onClick={() => {
          setAfter(undefined);
          state.retry();
        }}
      >
        Refresh candidates
      </Button>
      <Result state={state} timingLabel="Team member candidates">
        {(data) => (
          <>
            <ul className="divide-y rounded-md border bg-background">
              {data.items.map((member) => (
                <li
                  key={member.actor}
                  className="flex min-w-0 flex-wrap items-center justify-between gap-3 p-3"
                >
                  <Identity member={member} />
                  {member.role ? (
                    <Badge variant="outline">Already a {member.role}</Badge>
                  ) : (
                    <Button
                      variant="outline"
                      disabled={commands.locked || !data.can_manage}
                      onClick={() =>
                        void commands.submit(
                          organization,
                          actor,
                          {
                            kind: "member",
                            team,
                            member: member.actor,
                            version: member.version,
                            role: "member",
                          },
                          "Add team member",
                        )
                      }
                    >
                      Add member
                    </Button>
                  )}
                </li>
              ))}
            </ul>
            <TeamPages after={after} next={data.next} onPage={setAfter} />
          </>
        )}
      </Result>
    </section>
  );
}
function Identity({ member }: { member: Member }) {
  return (
    <div className="min-w-0 break-words">
      {member.profile ? (
        <Link href={`/-/users/${member.actor}`} className="font-medium">
          {member.profile.name}
        </Link>
      ) : (
        <strong className="font-medium">Member without a public profile</strong>
      )}
      <p className="mt-1 break-all text-sm text-muted-foreground">
        {member.profile ? `@${member.profile.handle}` : member.actor}
      </p>
    </div>
  );
}
function MemberRow({
  organization,
  actor,
  team,
  member,
  manage,
  commands,
}: {
  organization: string;
  actor: string;
  team: string;
  member: Member;
  manage: boolean;
  commands: Commands;
}) {
  const [role, setRole] = useState<Role>(member.role ?? "member");
  const [removing, setRemoving] = useState(false);
  return (
    <li className="flex flex-col gap-3 p-4">
      <div className="flex min-w-0 flex-wrap items-center justify-between gap-4">
        <Identity member={member} />
        <div className="flex flex-wrap items-center gap-2">
          {manage ? (
            <>
              <NativeSelect
                aria-label={`Team role for ${member.profile?.name ?? member.actor}`}
                value={role}
                disabled={commands.locked}
                onChange={(event) => setRole(event.target.value as Role)}
              >
                <NativeSelectOption value="member">Member</NativeSelectOption>
                <NativeSelectOption value="maintainer">
                  Maintainer
                </NativeSelectOption>
              </NativeSelect>
              <Button
                variant="outline"
                disabled={commands.locked || role === member.role}
                onClick={() =>
                  void commands.submit(
                    organization,
                    actor,
                    {
                      kind: "member",
                      team,
                      member: member.actor,
                      version: member.version,
                      role,
                    },
                    "Update team member role",
                  )
                }
              >
                Save role
              </Button>
              <Button
                variant="ghost"
                disabled={commands.locked}
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
            <p>Remove {member.profile?.name ?? "this member"} from the team?</p>
            <div className="mt-3 flex gap-2">
              <Button
                variant="destructive"
                disabled={commands.locked}
                onClick={() =>
                  void commands.submit(
                    organization,
                    actor,
                    {
                      kind: "member",
                      team,
                      member: member.actor,
                      version: member.version,
                      role: null,
                    },
                    "Remove team member",
                  )
                }
              >
                Remove team member
              </Button>
              <Button
                variant="outline"
                disabled={commands.locked}
                onClick={() => setRemoving(false)}
              >
                Keep member
              </Button>
            </div>
          </AlertDescription>
        </Alert>
      )}
    </li>
  );
}
export function TeamPages({
  after,
  next,
  onPage,
}: {
  after?: string;
  next: string | null;
  onPage: (value: string | undefined) => void;
}) {
  return (
    <nav aria-label="Team pagination" className="flex justify-center gap-2">
      <Button
        variant="outline"
        disabled={!after}
        onClick={() => onPage(undefined)}
      >
        First page
      </Button>
      <Button
        variant="outline"
        disabled={!next}
        onClick={() => onPage(next ?? undefined)}
      >
        Next page
      </Button>
    </nav>
  );
}
