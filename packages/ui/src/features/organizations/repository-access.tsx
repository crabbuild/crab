import { useState } from "react";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";
import { Empty, EmptyHeader, EmptyTitle } from "@/components/ui/empty";
import { useRequest } from "@/lib/api";
import type { MemberRepositoryAccess } from "@/lib/contracts";
import { repoHref } from "@/lib/repository-route";
import { repositoryRoleLabels as roleLabels } from "@/lib/repository-roles";
import { teamHref } from "./teams/model";

export function MemberAccess({
  organization,
  actor,
  name,
}: {
  organization: string;
  actor: string;
  name: string;
}) {
  return (
    <Dialog>
      <DialogTrigger asChild>
        <Button
          variant="outline"
          aria-label={`View repository access for ${name}`}
        >
          View repository access
        </Button>
      </DialogTrigger>
      <DialogContent className="max-h-[85dvh] grid-rows-[auto_minmax(0,1fr)] sm:max-w-2xl">
        <DialogHeader className="pr-8">
          <DialogTitle>Repository access</DialogTitle>
          <DialogDescription className="break-words">{name}</DialogDescription>
        </DialogHeader>
        <AccessList organization={organization} actor={actor} />
      </DialogContent>
    </Dialog>
  );
}

function AccessList({
  organization,
  actor,
}: {
  organization: string;
  actor: string;
}) {
  const [after, setAfter] = useState<string>();
  const query = new URLSearchParams(after ? { after } : {});
  const state = useRequest<MemberRepositoryAccess>(
    `/api/organizations/${organization}/members/${actor}/repository-access?${query}`,
  );
  return (
    <div className="flex min-w-0 flex-col gap-3 overflow-y-auto">
      <div className="flex flex-wrap gap-2">
        {after ? (
          <Button variant="outline" onClick={() => setAfter(undefined)}>
            First page
          </Button>
        ) : (
          <Button variant="outline" onClick={state.retry}>
            Refresh access
          </Button>
        )}
      </div>
      <Result state={state} timingLabel="Member repository access">
        {({ account, managed, items, next }) => (
          <>
            {managed && (
              <div>
                <Badge variant="outline">
                  Provisioned by identity provider
                </Badge>
              </div>
            )}
            {account !== "active" && (
              <Alert>
                <AlertDescription>
                  {account === "suspended"
                    ? "Sign-in and user Git credentials are blocked for this account. Its configured grants are shown below."
                    : "This member’s identity provider is unavailable on this server."}
                </AlertDescription>
              </Alert>
            )}
            {items.length === 0 ? (
              <Empty>
                <EmptyHeader>
                  <EmptyTitle>No repositories in this organization</EmptyTitle>
                </EmptyHeader>
              </Empty>
            ) : (
              <ul className="divide-y rounded-lg border">
                {items.map((repo) => (
                  <li className="grid gap-2 px-3 py-3" key={repo.id}>
                    <div className="flex min-w-0 flex-wrap items-center justify-between gap-2">
                      <Link
                        href={repoHref(repo)}
                        className="min-w-0 wrap-anywhere font-medium hover:underline"
                      >
                        {repo.name}
                      </Link>
                      <Badge variant="secondary">
                        {repo.access
                          ? roleLabels[repo.access]
                          : repo.sso_group_ceiling
                            ? "SSO-dependent"
                            : "No grant"}
                      </Badge>
                    </div>
                    {repo.inherited_from && (
                      <p className="text-muted-foreground">
                        Inherited from{" "}
                        <Link
                          className="wrap-anywhere underline"
                          href={repoHref({
                            owner: repo.owner,
                            name: repo.inherited_from,
                          })}
                        >
                          {repo.inherited_from}
                        </Link>
                      </p>
                    )}
                    <ul className="grid gap-1 text-muted-foreground">
                      {repo.organization_owner && <li>Organization owner</li>}
                      {repo.baseline && (
                        <li>
                          Organization baseline · {roleLabels[repo.baseline]}
                        </li>
                      )}
                      {repo.direct && (
                        <li>Direct grant · {roleLabels[repo.direct]}</li>
                      )}
                      {repo.teams.map((team) => (
                        <li className="wrap-anywhere" key={team.id}>
                          <Link
                            className="underline"
                            href={teamHref(organization, team.id)}
                          >
                            {team.name}
                          </Link>{" "}
                          · {roleLabels[team.access]}
                        </li>
                      ))}
                      {repo.sso_group_ceiling && (
                        <li>
                          SSO group grants up to{" "}
                          {roleLabels[repo.sso_group_ceiling]} apply only when
                          confirmed by the member’s sign-in session.
                        </li>
                      )}
                    </ul>
                  </li>
                ))}
              </ul>
            )}
            {next && (
              <Button
                className="self-start"
                variant="outline"
                onClick={() => setAfter(next)}
              >
                Next page
              </Button>
            )}
          </>
        )}
      </Result>
    </div>
  );
}
