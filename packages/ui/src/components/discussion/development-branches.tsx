import { useDeferredValue, useRef, useState, type ReactNode } from "react";
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
type NewBranch = { repository: string; name: string; source_oid: string };
type CreationRequest = NewBranch & { request_id: string };

export interface BranchLinks {
  items: BranchLink[];
  version: number;
  can_manage: boolean;
  pending?: CreationRequest | null;
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
  const [mode, setMode] = useState<"link" | "create">();
  const captured = useRef<{ key: string; request: CreationRequest }>(null);
  const mutation = useMutation(csrf, repo);
  const canManage = links.can_manage && !repo.archived;
  const route = endpoint(repo, `issues/${issue}/development-branches`);

  async function save(repository: string, reference: string, active: boolean) {
    const outcome = await mutation.run<{ version: number }>(route, "PUT", {
      branch: { repository, reference },
      active,
      version: links.version,
    });
    if (outcome) {
      setMode(undefined);
      onChanged();
    }
  }

  async function create(request: CreationRequest) {
    const outcome = await mutation.run<{ branch: string; commit: string }>(
      route,
      "POST",
      request,
    );
    if (outcome) {
      captured.current = null;
      setMode(undefined);
      onChanged();
    }
  }

  function startCreation(input: NewBranch) {
    const key = JSON.stringify([
      repo.id,
      issue,
      input.repository,
      input.name,
      input.source_oid,
    ]);
    if (captured.current?.key !== key) {
      captured.current = {
        key,
        request: { request_id: crypto.randomUUID(), ...input },
      };
    }
    void create(captured.current.request);
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
      <header className="flex flex-wrap items-center justify-between gap-2">
        <strong>Linked branches</strong>
        {canManage && (
          <div className="flex flex-wrap gap-1">
            <Button
              variant="outline"
              size="sm"
              type="button"
              disabled={mutation.pending || Boolean(links.pending)}
              onClick={() => {
                mutation.reset();
                setMode("create");
              }}
            >
              <GitBranchIcon data-icon="inline-start" />
              Create a branch
            </Button>
            <Button
              variant="ghost"
              size="sm"
              type="button"
              disabled={mutation.pending || Boolean(links.pending)}
              onClick={() => {
                mutation.reset();
                setMode("link");
              }}
            >
              Link branch
            </Button>
          </div>
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
      {canManage && links.pending && (
        <div className="flex flex-wrap items-center gap-2">
          <p className="muted">
            Branch creation pending for {links.pending.name}.
          </p>
          <Button
            size="sm"
            variant="outline"
            disabled={mutation.pending}
            onClick={() => {
              if (links.pending) void create(links.pending);
            }}
          >
            Retry branch creation
          </Button>
          <Button
            size="sm"
            variant="ghost"
            disabled={mutation.pending}
            onClick={onChanged}
          >
            Check status
          </Button>
        </div>
      )}
      {mode ? (
        <BranchPicker
          repo={repo}
          links={links.items}
          mode={mode}
          issue={issue}
          pending={mutation.pending}
          conflict={mutation.conflict}
          onClose={() => setMode(undefined)}
          onLink={(repository, reference) =>
            void save(repository, reference, true)
          }
          onCreate={startCreation}
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
  mode,
  issue,
  pending,
  conflict,
  onClose,
  onLink,
  onCreate,
  children,
}: {
  repo: RepositoryAddress;
  links: BranchLink[];
  mode: "create" | "link";
  issue: number;
  pending: boolean;
  conflict: boolean;
  onClose: () => void;
  onLink: (repository: string, reference: string) => void;
  onCreate: (input: NewBranch) => void;
  children: ReactNode;
}) {
  const creating = mode === "create";
  const [name, setName] = useState(`issue-${issue}`);
  const [repository, setRepository] = useState(repo);
  const [repositorySearch, setRepositorySearch] = useState("");
  const [branchSearch, setBranchSearch] = useState("");
  const [branch, setBranch] = useState("");
  const query = useDeferredValue(repositorySearch.trim());
  const candidates = useRequest<{ repositories: RepositoryAddress[] }>(
    `/api/repos?${new URLSearchParams({ q: query, limit: "20" })}`,
  );
  const refs = useRequest<Refs>(endpoint(repository, "refs"), repository.id);
  const sameRepository = repository.id === repo.id;
  const access = useRequest<{ repositories: Repository[] }>(
    creating && !sameRepository
      ? `/api/repos?${new URLSearchParams({ owner: repository.owner, name: repository.name })}`
      : null,
  );
  const writable =
    sameRepository ||
    access.data?.repositories.some(
      (item) =>
        item.id === repository.id && item.access === "write" && !item.archived,
    );
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
    (item) =>
      item.name.startsWith(branchesPrefix) &&
      (creating || !linked.has(item.name)),
  );
  const filter = branchSearch.trim().toLowerCase();
  const matches = available.filter((ref) =>
    ref.name.slice(branchesPrefix.length).toLowerCase().includes(filter),
  );
  const defaultBranch =
    creating && matches.find((ref) => ref.name === refs.data?.head?.name);
  const matching = defaultBranch
    ? [
        defaultBranch,
        ...matches.filter((ref) => ref !== defaultBranch).slice(0, 99),
      ]
    : matches.slice(0, 100);
  const selection = branch || (creating ? refs.data?.head?.name : "");
  const selected = matching.find((ref) => ref.name === selection);
  const canSubmit = Boolean(
    selected &&
    (!creating || (writable && name.trim().length)) &&
    !pending &&
    !conflict,
  );

  return (
    <Dialog
      open
      onOpenChange={(open) => {
        if (!open && !pending) onClose();
      }}
    >
      <DialogContent>
        <DialogHeader>
          <DialogTitle>
            {creating ? "Create a branch for this issue" : "Link a branch"}
          </DialogTitle>
          <DialogDescription>
            {creating
              ? "Choose the repository and starting branch."
              : "Choose an existing branch in a repository you can read."}
          </DialogDescription>
        </DialogHeader>
        <form
          className="flex flex-col gap-4"
          onSubmit={(event) => {
            event.preventDefault();
            if (!selected || !canSubmit) return;
            if (creating) {
              onCreate({
                repository: repository.id,
                name: name.trim(),
                source_oid: selected.oid,
              });
            } else {
              onLink(repository.id, selected.name);
            }
          }}
        >
          <FieldGroup>
            {creating && (
              <Field>
                <FieldLabel htmlFor="new-development-branch">
                  New branch name
                </FieldLabel>
                <Input
                  id="new-development-branch"
                  value={name}
                  required
                  maxLength={255}
                  disabled={pending}
                  onChange={(event) => setName(event.target.value)}
                />
              </Field>
            )}
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
            {creating &&
              !sameRepository &&
              (access.error || (access.data && !writable)) && (
                <p className="error" role="alert">
                  {access.error ??
                    "Write access is required in the chosen repository."}
                </p>
              )}
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
                    <FieldLabel htmlFor="development-branch">
                      {creating ? "Starting branch" : "Branch"}
                    </FieldLabel>
                    <NativeSelect
                      id="development-branch"
                      required
                      value={selected?.name ?? ""}
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
            <Button disabled={!canSubmit} type="submit">
              {creating ? "Create branch" : "Link branch"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
