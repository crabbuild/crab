import { useDeferredValue, useState } from "react";
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
import { WorkPicker } from "@/features/projects/item";
import type { Work } from "@/features/projects/model";
import { endpoint, useRequest, type Repository } from "@/lib/api";
import { repoHref } from "@/lib/repository-route";
import { useMutation } from "./mutations";

type RepositoryAddress = Pick<Repository, "id" | "owner" | "name">;
type LinkState = { active: boolean; version: number };
export type RelatedWork = {
  work: Work;
  repository: RepositoryAddress;
  version: number;
  can_unlink: boolean;
};

function linkEndpoint(
  source: RepositoryAddress,
  issue: number,
  target: RepositoryAddress,
  pull: number,
) {
  return endpoint(source, `issues/${issue}/linked-pulls/${target.id}/${pull}`);
}

export function CrossRepositoryLinks({
  items,
  repo,
  issue,
  pull,
  csrf,
  canManage,
  onSaved,
}: {
  items: RelatedWork[];
  repo: Repository;
  issue?: number;
  pull?: number;
  csrf: string;
  canManage: boolean;
  onSaved: () => void;
}) {
  const [open, setOpen] = useState(false);
  const mutation = useMutation(csrf, repo);
  async function unlink(row: RelatedWork) {
    const source = issue ? repo : row.repository;
    const target = issue ? row.repository : repo;
    const issueNumber = issue ?? row.work.subject.number;
    const pullNumber = pull ?? row.work.subject.number;
    const saved = await mutation.run<LinkState>(
      linkEndpoint(source, issueNumber, target, pullNumber),
      "PUT",
      { active: false, version: row.version },
      source,
    );
    if (saved) onSaved();
  }
  return (
    <>
      {items.length > 0 && (
        <ul aria-label="Cross-repository links">
          {items.map((row) => {
            const { repository, work } = row;
            const number = String(work.subject.number);
            const href = repoHref(repository, {
              view: issue ? "pulls" : "issues",
              issue: pull ? number : undefined,
              pull: issue ? number : undefined,
              repository_id: repository.id,
            });
            const title = `${repository.owner}/${repository.name}#${number}`;
            return (
              <li key={`${repository.id}:${number}`}>
                <Link href={href}>
                  {title} {work.title}
                </Link>
                <span className="muted">{work.state}</span>
                {(issue ? canManage : row.can_unlink) && (
                  <Button
                    type="button"
                    size="sm"
                    variant="ghost"
                    aria-label={`Unlink ${title}`}
                    disabled={mutation.pending}
                    onClick={() => void unlink(row)}
                  >
                    Unlink
                  </Button>
                )}
              </li>
            );
          })}
        </ul>
      )}
      {mutation.error && (
        <p className="error" role="alert">
          {mutation.error}
        </p>
      )}
      {mutation.conflict && (
        <Button type="button" size="sm" variant="outline" onClick={onSaved}>
          Reload linked work
        </Button>
      )}
      {issue && canManage && repo.organization_id && (
        <Button
          type="button"
          size="sm"
          variant="outline"
          onClick={() => setOpen(true)}
        >
          Link pull request
        </Button>
      )}
      {open && issue && repo.organization_id && (
        <LinkPicker
          repo={repo}
          organization={repo.organization_id}
          issue={issue}
          csrf={csrf}
          close={() => setOpen(false)}
          onSaved={() => {
            setOpen(false);
            onSaved();
          }}
        />
      )}
    </>
  );
}

function LinkPicker({
  repo,
  organization,
  issue,
  csrf,
  close,
  onSaved,
}: {
  repo: RepositoryAddress;
  organization: string;
  issue: number;
  csrf: string;
  close: () => void;
  onSaved: () => void;
}) {
  const [search, setSearch] = useState("");
  const [repository, setRepository] = useState<RepositoryAddress>();
  const [work, setWork] = useState<Work>();
  const query = useDeferredValue(search.trim());
  const names = new URLSearchParams({ organization, q: query, limit: "20" });
  const candidates = useRequest<{ repositories: RepositoryAddress[] }>(
    `/api/repos?${names}`,
  );
  const addresses = new Map(
    [
      ...(repository ? [repository] : []),
      ...(candidates.data?.repositories ?? []),
    ]
      .filter((candidate) => candidate.id !== repo.id)
      .map((candidate) => [candidate.id, candidate]),
  );
  const mutation = useMutation(csrf, repo);
  const url =
    repository && work
      ? linkEndpoint(repo, issue, repository, work.subject.number)
      : null;
  const pair = useRequest<LinkState>(url, repo.id);
  return (
    <Dialog
      open
      onOpenChange={(visible) => {
        if (!visible && !mutation.pending) close();
      }}
    >
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Link a pull request</DialogTitle>
          <DialogDescription>
            Choose related work in another repository in this organization.
          </DialogDescription>
        </DialogHeader>
        <form
          className="flex flex-col gap-4"
          onSubmit={async (event) => {
            event.preventDefault();
            if (!url || !pair.data || pair.data.active || mutation.pending)
              return;
            const saved = await mutation.run<LinkState>(url, "PUT", {
              active: true,
              version: pair.data.version,
            });
            if (saved) onSaved();
          }}
        >
          <FieldGroup>
            <Field>
              <FieldLabel htmlFor="linked-pull-repository-search">
                Find a repository
              </FieldLabel>
              <Input
                id="linked-pull-repository-search"
                value={search}
                maxLength={100}
                disabled={mutation.pending}
                onChange={(event) => setSearch(event.target.value)}
              />
            </Field>
            {candidates.error && (
              <p className="error" role="alert">
                {candidates.error}
              </p>
            )}
            <Field>
              <FieldLabel htmlFor="linked-pull-repository">
                Repository
              </FieldLabel>
              <NativeSelect
                id="linked-pull-repository"
                value={repository?.id ?? ""}
                disabled={mutation.pending}
                onChange={(event) => {
                  setRepository(addresses.get(event.target.value));
                  setWork(undefined);
                  mutation.reset();
                }}
              >
                <NativeSelectOption value="">
                  Choose a repository
                </NativeSelectOption>
                {[...addresses.values()].map((address) => (
                  <NativeSelectOption key={address.id} value={address.id}>
                    {address.owner}/{address.name}
                  </NativeSelectOption>
                ))}
              </NativeSelect>
            </Field>
            {repository && !work && (
              <WorkPicker
                key={repository.id}
                repo={repository}
                kind="pull"
                onSelect={setWork}
              />
            )}
            {work && (
              <div className="flex flex-col gap-2">
                <strong>
                  #{work.subject.number} {work.title}
                </strong>
                <Button
                  type="button"
                  size="sm"
                  variant="ghost"
                  disabled={mutation.pending}
                  onClick={() => {
                    setWork(undefined);
                    mutation.reset();
                  }}
                >
                  Choose another pull request
                </Button>
                <Result state={pair}>
                  {(state) =>
                    state.active && (
                      <p className="muted">
                        This pull request is already linked.
                      </p>
                    )
                  }
                </Result>
              </div>
            )}
          </FieldGroup>
          {mutation.error && (
            <p className="error" role="alert">
              {mutation.error}
            </p>
          )}
          {mutation.conflict && (
            <Button
              type="button"
              size="sm"
              variant="outline"
              onClick={() => {
                pair.retry();
                mutation.reset();
              }}
            >
              Reload the link
            </Button>
          )}
          <DialogFooter>
            <Button
              type="submit"
              disabled={
                !pair.data ||
                pair.data.active ||
                mutation.pending ||
                mutation.conflict
              }
            >
              Link pull request
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
