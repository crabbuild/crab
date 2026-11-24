import { useState } from "react";
import { useMutation } from "@/components/discussion/mutations";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";
import { Field, FieldLabel } from "@/components/ui/field";
import { FieldSelect } from "@/components/ui/field-select";
import { Input } from "@/components/ui/input";
import { endpoint, useRequest, type Repository } from "@/lib/api";
import type { RepositoryAccess, TeamAccessState } from "@/lib/contracts";
import { isRepositoryRole, repositoryRoleLabels } from "@/lib/repository-roles";
import { TeamPages } from "./members";

type Address = Pick<Repository, "id" | "owner" | "name">;

export function RepositoryGrant({
  organization,
  team,
  csrf,
  onSaved,
}: {
  organization: string;
  team: string;
  csrf: string;
  onSaved: (name: string) => void;
}) {
  const [open, setOpen] = useState(false);
  const [repository, setRepository] = useState<Address>();
  const mutation = useMutation(csrf, repository ?? null);
  const path = repository ? endpoint(repository, "team-access") : null;
  const grants = useRequest<TeamAccessState>(
    open ? path : null,
    repository?.id,
  );
  const [role, setRole] = useState<RepositoryAccess>();

  function close() {
    setOpen(false);
    setRepository(undefined);
    setRole(undefined);
    mutation.reset();
  }

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        if (next) setOpen(true);
        else if (!mutation.pending) close();
      }}
    >
      <DialogTrigger asChild>
        <Button>Grant repository access</Button>
      </DialogTrigger>
      <DialogContent className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>Grant repository access</DialogTitle>
          <DialogDescription>
            Choose a repository and the role this team should have.
          </DialogDescription>
        </DialogHeader>
        {open && !repository && (
          <RepositoryChoices
            organization={organization}
            onSelect={setRepository}
          />
        )}
        {repository && path && (
          <>
            <p className="font-medium wrap-anywhere">{repository.name}</p>
            <Result state={grants} timingLabel="Repository grants">
              {(data) => {
                if (data.organization.id !== organization)
                  return (
                    <p role="alert">
                      This repository changed organizations. Choose it again
                      from the list.
                    </p>
                  );
                const previous = data.organization.teams.find(
                  (grant) => grant.team === team,
                );
                const nextRole = role ?? previous?.access ?? "read";
                const reload =
                  mutation.status === 409 ||
                  mutation.status === 403 ||
                  mutation.status === 404;
                return (
                  <form
                    className="grid gap-4"
                    onSubmit={async (event) => {
                      event.preventDefault();
                      const teams = data.organization.teams.filter(
                        (grant) => grant.team !== team,
                      );
                      const saved = await mutation.run<TeamAccessState>(
                        path,
                        "PUT",
                        {
                          expected_revision: data.revision,
                          teams: [...teams, { team, access: nextRole }],
                        },
                      );
                      if (
                        saved?.organization.id === organization &&
                        saved.organization.teams.some(
                          (grant) =>
                            grant.team === team && grant.access === nextRole,
                        )
                      ) {
                        onSaved(repository.name);
                        close();
                      }
                    }}
                  >
                    <Field>
                      <FieldLabel htmlFor="team-repository-role">
                        Repository role
                      </FieldLabel>
                      <FieldSelect
                        id="team-repository-role"
                        label="Repository role"
                        value={nextRole}
                        options={repositoryRoleLabels}
                        disabled={reload || mutation.pending}
                        onValueChange={(value) => {
                          if (isRepositoryRole(value)) setRole(value);
                        }}
                      />
                    </Field>
                    {previous && (
                      <p className="text-muted-foreground">
                        Current grant {repositoryRoleLabels[previous.access]}
                      </p>
                    )}
                    {mutation.error && (
                      <p role="alert">
                        {reload
                          ? "Repository grants or permissions changed. Review the latest grants before saving."
                          : mutation.error}
                      </p>
                    )}
                    <div className="flex flex-wrap gap-2">
                      <Button
                        type="submit"
                        disabled={
                          reload ||
                          mutation.pending ||
                          previous?.access === nextRole
                        }
                      >
                        {mutation.pending ? "Saving…" : "Save grant"}
                      </Button>
                      {reload && (
                        <Button
                          variant="outline"
                          onClick={() => {
                            mutation.reset();
                            grants.retry();
                          }}
                        >
                          Reload grants
                        </Button>
                      )}
                    </div>
                  </form>
                );
              }}
            </Result>
            <Button
              variant="ghost"
              className="w-fit"
              disabled={mutation.pending}
              onClick={() => {
                setRepository(undefined);
                setRole(undefined);
                mutation.reset();
              }}
            >
              Choose another repository
            </Button>
          </>
        )}
      </DialogContent>
    </Dialog>
  );
}

function RepositoryChoices({
  organization,
  onSelect,
}: {
  organization: string;
  onSelect: (repo: Address) => void;
}) {
  const [query, setQuery] = useState("");
  const [search, setSearch] = useState("");
  const [after, setAfter] = useState<string>();
  const params = new URLSearchParams({
    organization,
    limit: "20",
    q: [search, "fork:false"].join(" ").trim(),
  });
  if (after) params.set("after", after);
  const state = useRequest<{
    repositories: (Address & { fork: boolean })[];
    next: string | null;
  }>(`/api/repos?${params}`);
  return (
    <>
      <form
        className="flex items-end gap-2"
        onSubmit={(event) => {
          event.preventDefault();
          if (!after && query.trim() === search) state.retry();
          setAfter(undefined);
          setSearch(query.trim());
        }}
      >
        <Field className="min-w-0">
          <FieldLabel htmlFor="team-repository-search">
            Find a repository
          </FieldLabel>
          <Input
            id="team-repository-search"
            type="search"
            value={query}
            maxLength={180}
            onChange={(event) => setQuery(event.target.value)}
          />
        </Field>
        <Button type="submit" variant="outline">
          Search
        </Button>
      </form>
      <Result state={state} timingLabel="Organization repositories">
        {(data) => (
          <>
            <ul
              aria-label="Available repositories"
              className="max-h-[35dvh] divide-y overflow-auto rounded-lg border"
            >
              {data.repositories
                .filter((item) => !item.fork)
                .map((repo) => (
                  <li key={repo.id} className="p-1">
                    <Button
                      variant="ghost"
                      className="h-auto w-full justify-start text-start whitespace-normal wrap-anywhere"
                      onClick={() => onSelect(repo)}
                    >
                      {repo.name}
                    </Button>
                  </li>
                ))}
            </ul>
            {!data.repositories.length && (
              <p role="status">No matching repositories.</p>
            )}
            <TeamPages after={after} next={data.next} onPage={setAfter} />
          </>
        )}
      </Result>
    </>
  );
}
