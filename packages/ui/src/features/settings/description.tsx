import { useId, useState } from "react";
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
import { useRequest, type Repository } from "@/lib/api";

type Description = { description: string; version: number };

export function DescriptionSettings({
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
      aria-labelledby="repository-description"
    >
      <header>
        <div>
          <h3 id="repository-description">Repository description</h3>
          <p className="break-words">{repo.description || "No description"}</p>
        </div>
        {!repo.archived && (
          <Dialog open={open} onOpenChange={setOpen}>
            <DialogTrigger asChild>
              <Button type="button" variant="outline" size="sm">
                Edit description
              </Button>
            </DialogTrigger>
            <DialogContent>
              <DialogHeader>
                <DialogTitle>Edit repository description</DialogTitle>
                <DialogDescription>
                  Use one line of up to 350 characters.
                </DialogDescription>
              </DialogHeader>
              {open && (
                <DescriptionForm
                  repo={repo}
                  csrf={csrf}
                  onSaved={() => {
                    setOpen(false);
                    onRepositoryChanged();
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

function DescriptionForm({
  repo,
  csrf,
  onSaved,
}: {
  repo: Repository;
  csrf: string;
  onSaved: () => void;
}) {
  const path = `/api/repositories/${repo.id}/description`;
  const settings = useRequest<Description>(path);
  const mutation = useMutation(csrf, repo);
  const [draft, setDraft] = useState<string>();
  const id = useId();
  const stale = mutation.code === "repository_description_changed";
  return (
    <Result state={settings} timingLabel="Repository description">
      {(current) => {
        const description = draft ?? current.description;
        const valid =
          [...description].length <= 350 && !/\p{Cc}/u.test(description);
        const canSave =
          valid &&
          description !== current.description &&
          !stale &&
          !mutation.pending;
        return (
          <form
            className="grid gap-3"
            onSubmit={async (event) => {
              event.preventDefault();
              if (!canSave) return;
              const saved = await mutation.run<Description>(path, "PUT", {
                description,
                version: current.version,
              });
              if (saved) onSaved();
            }}
          >
            <label htmlFor={id} className="text-sm font-medium">
              Description
            </label>
            <Input
              id={id}
              value={description}
              disabled={mutation.pending}
              onChange={(event) => setDraft(event.target.value)}
            />
            {!valid && (
              <p role="status">Use one line of up to 350 characters.</p>
            )}
            {mutation.error && <p role="alert">{mutation.error}</p>}
            {stale && (
              <Button
                type="button"
                variant="outline"
                className="w-fit"
                onClick={() => {
                  setDraft(undefined);
                  mutation.reset();
                  settings.retry();
                }}
              >
                Reload description
              </Button>
            )}
            <Button
              type="submit"
              className="justify-self-end"
              disabled={!canSave}
            >
              {mutation.pending ? "Saving…" : "Save description"}
            </Button>
          </form>
        );
      }}
    </Result>
  );
}
