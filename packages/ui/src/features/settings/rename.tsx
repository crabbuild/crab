import { useId, useRef, useState } from "react";
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
import { Input } from "@/components/ui/input";
import { navigate, useRequest, type Repository } from "@/lib/api";
import { repoHref } from "@/lib/repository-route";

type Name = Pick<Repository, "id" | "owner" | "name"> & { version: number };
type Intent = { name: string; version: number; request_id: string };

export function RenameSettings({
  repo,
  csrf,
  onRepositoryChanged,
}: {
  repo: Repository;
  csrf: string;
  onRepositoryChanged: () => void;
}) {
  const [open, setOpen] = useState(false);
  return (
    <section
      className="settings-section"
      aria-labelledby="repository-name-heading"
    >
      <header>
        <div>
          <h3 id="repository-name-heading">Repository name</h3>
          <p>
            <strong>{repo.name}</strong>
          </p>
        </div>
        {!repo.archived && (
          <Dialog open={open} onOpenChange={setOpen}>
            <DialogTrigger asChild>
              <Button type="button" variant="outline" size="sm">
                Rename repository
              </Button>
            </DialogTrigger>
            <DialogContent>
              <DialogHeader>
                <DialogTitle>Rename repository</DialogTitle>
                <DialogDescription>
                  Existing links and Git remotes will follow the new address
                  while the old name is available.
                </DialogDescription>
              </DialogHeader>
              {open && (
                <RenameForm
                  repo={repo}
                  csrf={csrf}
                  onRenamed={(renamed) => {
                    setOpen(false);
                    onRepositoryChanged();
                    navigate(repoHref(renamed, { view: "settings" }));
                  }}
                />
              )}
            </DialogContent>
          </Dialog>
        )}
      </header>
    </section>
  );
}

function RenameForm({
  repo,
  csrf,
  onRenamed,
}: {
  repo: Repository;
  csrf: string;
  onRenamed: (name: Name) => void;
}) {
  const path = `/api/repositories/${repo.id}/name`;
  const settings = useRequest<Name>(path);
  const mutation = useMutation(csrf, repo);
  const [draft, setDraft] = useState<string>();
  const [unconfirmed, setUnconfirmed] = useState(false);
  const intent = useRef<Intent | undefined>(undefined);
  const inputId = useId();
  return (
    <Result state={settings} timingLabel="Repository name">
      {(current) => {
        const name = draft ?? current.name;
        const valid =
          /^[A-Za-z0-9._-]{1,100}$/.test(name) && name !== "." && name !== "..";
        const stale = mutation.code === "page_changed";
        return (
          <form
            className="grid gap-3"
            onSubmit={async (event) => {
              event.preventDefault();
              if (name === current.name || !valid || stale) return;
              if (
                intent.current?.name !== name ||
                intent.current.version !== current.version
              ) {
                intent.current = {
                  name,
                  version: current.version,
                  request_id: crypto.randomUUID(),
                };
              }
              setUnconfirmed(false);
              const renamed = await mutation.run<Name | null>(
                path,
                "PUT",
                intent.current,
              );
              if (renamed) onRenamed(renamed);
              else if (renamed === null) setUnconfirmed(true);
            }}
          >
            <label htmlFor={inputId} className="text-sm font-medium">
              New repository name
            </label>
            <Input
              id={inputId}
              name="repository-name"
              value={name}
              disabled={mutation.pending || stale}
              maxLength={100}
              onChange={(event) => setDraft(event.target.value)}
            />
            {!valid && (
              <p className="text-sm">
                Use letters, numbers, periods, hyphens, or underscores.
              </p>
            )}
            {unconfirmed && (
              <p role="alert">
                The response was lost. Retry to confirm the saved repository
                name.
              </p>
            )}
            {mutation.error && (
              <p role="alert">
                {stale
                  ? "The repository name changed. Reload the current name."
                  : mutation.code === "repository_name_unavailable"
                    ? "That repository name is already in use."
                    : mutation.error}
              </p>
            )}
            {stale && (
              <Button
                type="button"
                variant="outline"
                className="w-fit"
                onClick={() => {
                  intent.current = undefined;
                  setDraft(undefined);
                  mutation.reset();
                  settings.retry();
                }}
              >
                Reload name
              </Button>
            )}
            <Button
              type="submit"
              className="justify-self-end"
              disabled={
                !valid || name === current.name || mutation.pending || stale
              }
            >
              {mutation.pending ? "Renaming…" : "Save name"}
            </Button>
          </form>
        );
      }}
    </Result>
  );
}
