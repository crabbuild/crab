import { useDeferredValue, useState } from "react";
import { GitBranchIcon } from "lucide-react";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Field, FieldGroup, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import {
  NativeSelect,
  NativeSelectOption,
} from "@/components/ui/native-select";
import { endpoint, useRequest, type Refs, type Repository } from "@/lib/api";
import { repoHref } from "@/lib/repository-route";
import { useMutation } from "./mutations";

type RepositoryAddress = Pick<Repository, "id" | "owner" | "name">;
type BranchLink = {
  repository: RepositoryAddress;
  reference: string;
  exists: boolean;
};

export interface BranchLinks {
  items: BranchLink[];
  version: number;
  can_manage: boolean;
}

const branchesPrefix = "refs/heads/";

export function DevelopmentBranches({
  repo,
  issue,
  csrf,
  links,
  onChanged,
}: {
  repo: Repository;
  issue: number;
  csrf: string;
  links: BranchLinks;
  onChanged: () => void;
}) {
  const [adding, setAdding] = useState(false);
  const mutation = useMutation(csrf, repo);
  const canManage = links.can_manage && !repo.archived;

  async function save(repository: string, reference: string, active: boolean) {
    const outcome = await mutation.run<{ version: number }>(
      endpoint(repo, `issues/${issue}/development-branches`),
      "PUT",
      { branch: { repository, reference }, active, version: links.version },
    );
    if (outcome) {
      setAdding(false);
      onChanged();
    }
  }

  const feedback = (
    <>
      {mutation.error && (
        <p role="alert" className="error">
          {mutation.error}
        </p>
      )}
      {mutation.conflict && (
        <Button type="button" variant="outline" size="sm" onClick={onChanged}>
          Reload linked branches
        </Button>
      )}
    </>
  );

  return (
    <section aria-label="Development branches" className="mt-3 border-t pt-3">
      <header className="flex items-center justify-between gap-2">
        <strong>Linked branches</strong>
        {canManage && (
          <Button
            variant="outline"
            size="sm"
            type="button"
            disabled={mutation.pending}
            onClick={() => {
              mutation.reset();
              setAdding(true);
            }}
          >
            <GitBranchIcon data-icon="inline-start" />
            Link branch
          </Button>
        )}
      </header>
      {links.items.length > 0 ? (
        <ul>
          {links.items.map(({ repository, reference, exists }) => {
            const name = reference.slice(branchesPrefix.length);
            const label = `${repository.owner}/${repository.name}:${name}`;
            return (
              <li key={`${repository.id}:${reference}`}>
                {exists ? (
                  <Link
                    href={repoHref(repository, {
                      rev: reference,
                      repository_id: repository.id,
                    })}
                    className="break-all"
                  >
                    {label}
                  </Link>
                ) : (
                  <span className="muted break-all">
                    {label} (branch unavailable)
                  </span>
                )}
                {canManage && (
                  <Button
                    type="button"
                    variant="ghost"
                    size="sm"
                    aria-label={`Unlink ${label}`}
                    disabled={mutation.pending}
                    onClick={() => void save(repository.id, reference, false)}
                  >
                    Unlink
                  </Button>
                )}
              </li>
            );
          })}
        </ul>
      ) : (
        <p className="muted">No linked branches.</p>
      )}
      {adding ? (
        <BranchPicker
          repo={repo}
          links={links.items}
          pending={mutation.pending}
          conflict={mutation.conflict}
          onClose={() => setAdding(false)}
          onSelect={(repository, reference) =>
            void save(repository, reference, true)
          }
        >
          {feedback}
        </BranchPicker>
      ) : (
        feedback
      )}
    </section>
  );
}

function BranchPicker({
  repo,
  links,
  pending,
  conflict,
  onClose,
  onSelect,
  children,
}: {
  repo: RepositoryAddress;
  links: BranchLink[];
  pending: boolean;
  conflict: boolean;
  onClose: () => void;
  onSelect: (repository: string, reference: string) => void;
  children: React.ReactNode;
}) {
  const [repository, setRepository] = useState(repo);
  const [repositorySearch, setRepositorySearch] = useState("");
  const [branchSearch, setBranchSearch] = useState("");
  const [branch, setBranch] = useState("");
  const query = useDeferredValue(repositorySearch.trim());
  const candidates = useRequest<{ repositories: RepositoryAddress[] }>(
    `/api/repos?${new URLSearchParams({ q: query, limit: "20" })}`,
  );
  const refs = useRequest<Refs>(endpoint(repository, "refs"), repository.id);
  const repositories = new Map(
    [repository, ...(candidates.data?.repositories ?? [])].map((item) => [
      item.id,
      item,
    ]),
  );
  const linked = new Set(
    links
      .filter((link) => link.repository.id === repository.id)
      .map((link) => link.reference),
  );
  const available = (refs.data?.refs ?? []).filter(
    (item) => item.name.startsWith(branchesPrefix) && !linked.has(item.name),
  );
  const filter = branchSearch.trim().toLowerCase();
  const matching = available
    .filter((ref) =>
      ref.name.slice(branchesPrefix.length).toLowerCase().includes(filter),
    )
    .slice(0, 100);
  const selected = matching.some((ref) => ref.name === branch) ? branch : "";

  return (
    <Dialog
      open
      onOpenChange={(open) => {
        if (!open && !pending) onClose();
      }}
    >
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Link a branch</DialogTitle>
          <DialogDescription>
            Choose an existing branch in a repository you can read.
          </DialogDescription>
        </DialogHeader>
        <form
          className="flex flex-col gap-4"
          onSubmit={(event) => {
            event.preventDefault();
            if (selected && !pending && !conflict)
              onSelect(repository.id, selected);
          }}
        >
          <FieldGroup>
            <Field>
              <FieldLabel htmlFor="branch-repository-search">
                Find a repository
              </FieldLabel>
              <Input
                id="branch-repository-search"
                value={repositorySearch}
                maxLength={100}
                disabled={pending}
                onChange={(event) => setRepositorySearch(event.target.value)}
              />
            </Field>
            {candidates.error && (
              <p className="error" role="alert">
                {candidates.error}
              </p>
            )}
            <Field>
              <FieldLabel htmlFor="branch-repository">Repository</FieldLabel>
              <NativeSelect
                id="branch-repository"
                value={repository.id}
                disabled={pending}
                onChange={(event) => {
                  const choice = repositories.get(event.target.value);
                  if (!choice) return;
                  setRepository(choice);
                  setBranch("");
                  setBranchSearch("");
                }}
              >
                {[...repositories.values()].map((item) => (
                  <NativeSelectOption key={item.id} value={item.id}>
                    {item.owner}/{item.name}
                  </NativeSelectOption>
                ))}
              </NativeSelect>
            </Field>
            <Result state={refs} timingLabel="Branches">
              {() => (
                <>
                  <Field>
                    <FieldLabel htmlFor="development-branch-search">
                      Find a branch
                    </FieldLabel>
                    <Input
                      id="development-branch-search"
                      value={branchSearch}
                      disabled={pending}
                      onChange={(event) => setBranchSearch(event.target.value)}
                    />
                  </Field>
                  <Field>
                    <FieldLabel htmlFor="development-branch">Branch</FieldLabel>
                    <NativeSelect
                      id="development-branch"
                      required
                      value={selected}
                      disabled={pending || !matching.length}
                      onChange={(event) => setBranch(event.target.value)}
                    >
                      <NativeSelectOption value="">
                        {matching.length
                          ? "Choose a branch"
                          : "No matching branches"}
                      </NativeSelectOption>
                      {matching.map((item) => (
                        <NativeSelectOption key={item.name} value={item.name}>
                          {item.name.slice(branchesPrefix.length)}
                        </NativeSelectOption>
                      ))}
                    </NativeSelect>
                  </Field>
                </>
              )}
            </Result>
          </FieldGroup>
          {children}
          <DialogFooter>
            <Button disabled={!selected || pending || conflict} type="submit">
              Link branch
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
