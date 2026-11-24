import { useState } from "react";
import {
  BookMarkedIcon,
  SearchIcon,
  UserRoundIcon,
  UserRoundMinusIcon,
} from "lucide-react";
import { useMutation } from "@/components/discussion/mutations";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { Avatar, AvatarFallback } from "@/components/ui/avatar";
import { Badge } from "@/components/ui/badge";
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
  DialogTrigger,
} from "@/components/ui/dialog";
import {
  Collapsible,
  CollapsibleContent,
  CollapsibleTrigger,
} from "@/components/ui/collapsible";
import {
  Empty,
  EmptyDescription,
  EmptyHeader,
  EmptyMedia,
  EmptyTitle,
} from "@/components/ui/empty";
import { Field, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { useRequest } from "@/lib/api";
import type {
  OrganizationPage,
  OutsideCollaborator,
  OutsideCollaboratorRepository,
  OutsideGrantChange,
} from "@/lib/contracts";
import { repositoryRoleLabels } from "@/lib/repository-roles";
import { repoHref } from "@/lib/repository-route";
import { InviteDialog } from "./invitations";
import { MutationError } from "./feedback";

export function OutsideCollaborators({
  endpoint,
  csrf,
  onInvited,
}: {
  endpoint: string;
  csrf: string;
  onInvited: () => void;
}) {
  const [search, setSearch] = useState("");
  const [q, setQ] = useState("");
  const [after, setAfter] = useState<string>();
  const [expanded, setExpanded] = useState<string>();
  const [checked, setChecked] = useState<string[]>([]);
  const [notice, setNotice] = useState("");
  const parameters = new URLSearchParams(q ? { q } : {});
  if (after) parameters.set("after", after);
  const root = endpoint + "/outside-collaborators";
  const list = useRequest<OrganizationPage<OutsideCollaborator>>(
    `${root}?${parameters}`,
  );
  const firstPage = () => {
    setAfter(undefined);
    setExpanded(undefined);
    setChecked([]);
    if (!after) list.retry();
  };
  return (
    <section aria-labelledby="outside-collaborators" className="grid gap-4">
      <h2 id="outside-collaborators" className="text-lg font-semibold">
        Outside collaborators
      </h2>
      <form
        role="search"
        aria-label="Outside collaborator search"
        className="flex flex-wrap items-end gap-2"
        onSubmit={(event) => {
          event.preventDefault();
          setExpanded(undefined);
          setAfter(undefined);
          setChecked([]);
          setQ(search.trim());
        }}
      >
        <Field className="min-w-0 flex-1 sm:max-w-xs">
          <FieldLabel htmlFor="collaborator-search">
            Find collaborators
          </FieldLabel>
          <Input
            id="collaborator-search"
            type="search"
            placeholder="Name or username"
            autoComplete="off"
            maxLength={100}
            value={search}
            onChange={(event) => setSearch(event.target.value)}
          />
        </Field>
        <Button type="submit" variant="outline">
          <SearchIcon data-icon="inline-start" />
          Search
        </Button>
        <Button type="button" variant="ghost" onClick={firstPage}>
          {after ? "First page" : "Refresh"}
        </Button>
      </form>
      {notice && (
        <p role="status" className="text-sm">
          {notice}
        </p>
      )}
      <Result state={list} timingLabel="Outside collaborators">
        {({ items, next, revision }) => (
          <>
            {checked.length > 0 && (
              <RemoveCollaborators
                root={root}
                csrf={csrf}
                people={items.filter((person) => checked.includes(person.key))}
                revision={revision}
                onRefresh={firstPage}
                onRemoved={({ collaborators, grants }) => {
                  setNotice(
                    `Removed ${grants} direct repository ${grants === 1 ? "grant" : "grants"} for ${collaborators} ${collaborators === 1 ? "collaborator" : "collaborators"}.`,
                  );
                  firstPage();
                }}
              />
            )}
            {items.length ? (
              <ul className="divide-y rounded-lg border">
                {items.map((person) => (
                  <li key={person.key} className="p-3 sm:p-4">
                    <Collapsible
                      open={expanded === person.key}
                      onOpenChange={(open) =>
                        setExpanded(open ? person.key : undefined)
                      }
                    >
                      <div className="flex min-w-0 flex-wrap items-center gap-3">
                        <Checkbox
                          aria-label={`Select ${person.name}`}
                          checked={checked.includes(person.key)}
                          onCheckedChange={(selected) =>
                            setChecked((prior) =>
                              selected
                                ? [...prior, person.key]
                                : prior.filter((key) => key !== person.key),
                            )
                          }
                        />
                        <Avatar aria-hidden="true">
                          <AvatarFallback>
                            <UserRoundIcon className="size-4" />
                          </AvatarFallback>
                        </Avatar>
                        <div className="min-w-0 flex-1 basis-32">
                          {person.profile ? (
                            <>
                              <Link
                                href={`/-/users/${person.profile.id}`}
                                className="wrap-anywhere font-medium hover:underline"
                              >
                                {person.name}
                              </Link>
                              <p className="wrap-anywhere text-sm text-muted-foreground">
                                @{person.profile.handle}
                              </p>
                            </>
                          ) : (
                            <strong className="wrap-anywhere font-medium">
                              {person.name}
                            </strong>
                          )}
                        </div>
                        <div className="flex flex-wrap items-center gap-2">
                          {person.profile && (
                            <InviteDialog
                              endpoint={endpoint}
                              csrf={csrf}
                              recipient={person.profile}
                              onInvited={onInvited}
                            />
                          )}
                          <CollapsibleTrigger asChild>
                            <Button
                              variant="outline"
                              aria-label={`View repository access for ${person.name}`}
                            >
                              <BookMarkedIcon data-icon="inline-start" />
                              {person.repository_count}{" "}
                              {person.repository_count === 1
                                ? "repository"
                                : "repositories"}
                            </Button>
                          </CollapsibleTrigger>
                        </div>
                      </div>
                      <CollapsibleContent>
                        {expanded === person.key && (
                          <CollaboratorRepositories
                            path={`${root}/${person.key}/repositories`}
                          />
                        )}
                      </CollapsibleContent>
                    </Collapsible>
                  </li>
                ))}
              </ul>
            ) : (
              <Empty className="border">
                <EmptyHeader>
                  <EmptyMedia variant="icon">
                    <UserRoundIcon />
                  </EmptyMedia>
                  <EmptyTitle>
                    {next
                      ? "No matches on this page"
                      : q
                        ? "No matching collaborators"
                        : "No outside collaborators"}
                  </EmptyTitle>
                  <EmptyDescription>
                    {next
                      ? "Continue to look through the remaining grants."
                      : q
                        ? "Try a different name or username."
                        : "People with direct repository access who aren’t organization members appear here."}
                  </EmptyDescription>
                </EmptyHeader>
              </Empty>
            )}
            {next && (
              <Button
                className="justify-self-start"
                variant="outline"
                onClick={() => {
                  setExpanded(undefined);
                  setChecked([]);
                  setAfter(next);
                }}
              >
                Next page
              </Button>
            )}
          </>
        )}
      </Result>
    </section>
  );
}

function RemoveCollaborators({
  root,
  csrf,
  people,
  revision,
  onRefresh,
  onRemoved,
}: {
  root: string;
  csrf: string;
  people: OutsideCollaborator[];
  revision: number;
  onRefresh: () => void;
  onRemoved: (removal: OutsideGrantChange) => void;
}) {
  const [open, setOpen] = useState(false);
  const mutation = useMutation(csrf, null);
  async function remove() {
    const result = await mutation.run<OutsideGrantChange>(
      `${root}/remove`,
      "POST",
      {
        revision,
        keys: people.map((person) => person.key),
      },
    );
    if (result) {
      setOpen(false);
      onRemoved(result);
    }
  }
  return (
    <Dialog
      open={open}
      onOpenChange={(value) => {
        if (mutation.pending) return;
        mutation.reset();
        setOpen(value);
      }}
    >
      <div className="flex flex-wrap items-center gap-3">
        <span className="text-sm text-muted-foreground">
          {people.length} selected
        </span>
        <DialogTrigger asChild>
          <Button variant="outline" disabled={!people.length}>
            <UserRoundMinusIcon data-icon="inline-start" />
            Remove from all repositories
          </Button>
        </DialogTrigger>
      </div>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Remove outside collaborators</DialogTitle>
          <DialogDescription>
            Remove direct access for these collaborators across this
            organization. Access through signed identity-provider groups can
            still apply.
          </DialogDescription>
        </DialogHeader>
        <ul className="max-h-48 divide-y overflow-y-auto rounded-md border">
          {people.map((person) => (
            <li
              key={person.key}
              className="flex flex-wrap items-center justify-between gap-2 px-3 py-2"
            >
              <span className="min-w-0 wrap-anywhere font-medium">
                {person.name}
                {person.profile && (
                  <span className="ml-2 text-sm font-normal text-muted-foreground">
                    @{person.profile.handle}
                  </span>
                )}
              </span>
              <Badge variant="secondary">
                {person.repository_count}{" "}
                {person.repository_count === 1 ? "repository" : "repositories"}
              </Badge>
            </li>
          ))}
        </ul>
        <MutationError error={mutation.error} />
        <DialogFooter>
          <DialogClose asChild>
            <Button variant="outline" disabled={mutation.pending}>
              Cancel
            </Button>
          </DialogClose>
          {mutation.status === 409 ? (
            <Button onClick={onRefresh}>Reload collaborators</Button>
          ) : (
            <Button
              variant="destructive"
              disabled={mutation.pending}
              onClick={() => void remove()}
            >
              {mutation.pending ? "Removing…" : "Remove outside collaborators"}
            </Button>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

function CollaboratorRepositories({ path }: { path: string }) {
  const [after, setAfter] = useState<string>();
  const parameters = new URLSearchParams(after ? { after } : {});
  const list = useRequest<OrganizationPage<OutsideCollaboratorRepository>>(
    `${path}?${parameters}`,
  );
  return (
    <div className="mt-3 grid gap-2 border-t pt-3">
      <Result state={list} timingLabel="Collaborator repository access">
        {({ items, next }) => (
          <>
            <ul aria-label="Collaborator repositories" className="grid gap-2">
              {items.map((repo) => (
                <li
                  key={repo.id}
                  className="flex flex-wrap items-center gap-2 pl-1"
                >
                  <Link
                    className="min-w-0 flex-1 basis-24 wrap-anywhere font-medium hover:underline"
                    href={repoHref(repo)}
                  >
                    {repo.name}
                  </Link>
                  <Badge variant="secondary">
                    {repositoryRoleLabels[repo.access]}
                  </Badge>
                  <Button asChild size="sm" variant="ghost">
                    <Link
                      href={repoHref(repo, {
                        view: "settings",
                        section: "members",
                        repository_id: repo.id,
                      })}
                      aria-label={`Manage access to ${repo.name}`}
                    >
                      Manage access
                    </Link>
                  </Button>
                </li>
              ))}
            </ul>
            {next && (
              <Button
                variant="outline"
                className="justify-self-start"
                onClick={() => setAfter(next)}
              >
                More repositories
              </Button>
            )}
          </>
        )}
      </Result>
      {after && (
        <Button
          variant="ghost"
          className="justify-self-start"
          onClick={() => setAfter(undefined)}
        >
          First repository page
        </Button>
      )}
    </div>
  );
}
