import { BellIcon } from "lucide-react";
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
import {
  NativeSelect,
  NativeSelectOption,
} from "@/components/ui/native-select";
import { refreshNotificationBadge } from "@/features/notifications";
import { endpoint, useRequest, type Repository } from "@/lib/api";

const modes = {
  direct: "Direct notifications",
  issues: "Issue updates",
  pull_requests: "Pull request updates",
  issues_and_pull_requests: "Issue and pull request updates",
  muted: "Muted",
};

type Mode = keyof typeof modes;
type Preference = { mode: Mode; version: number };

export function RepositoryWatch({
  repo,
  csrf,
}: {
  repo: Repository;
  csrf: string;
}) {
  const [open, setOpen] = useState(false);
  const [draft, setDraft] = useState<Mode>();
  const path = endpoint(repo, "subscription");
  const preference = useRequest<Preference>(open ? path : null);
  const mutation = useMutation(csrf, repo);

  function close(next: boolean) {
    setOpen(next);
    if (!next) {
      setDraft(undefined);
      mutation.reset();
    }
  }

  return (
    <Dialog open={open} onOpenChange={close}>
      <DialogTrigger asChild>
        <Button variant="outline" size="sm" className="shrink-0">
          <BellIcon data-icon="inline-start" />
          Watch
        </Button>
      </DialogTrigger>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Repository notifications</DialogTitle>
          <DialogDescription>
            Choose what appears in your inbox for {repo.owner}/{repo.name}.
            Direct notifications include assignments, review requests, and
            mentions.
          </DialogDescription>
        </DialogHeader>
        <Result state={preference} timingLabel="Repository subscription">
          {(saved) => (
            <form
              className="grid gap-3"
              onSubmit={async (event) => {
                event.preventDefault();
                const mode = draft ?? saved.mode;
                const updated = await mutation.run<Preference>(path, "PUT", {
                  mode,
                  version: saved.version,
                });
                if (updated) {
                  refreshNotificationBadge();
                  close(false);
                }
              }}
            >
              <label className="text-sm font-medium" htmlFor="watch-mode">
                Notify me about
              </label>
              <NativeSelect
                id="watch-mode"
                value={draft ?? saved.mode}
                disabled={mutation.pending}
                onChange={(event) => setDraft(event.target.value as Mode)}
              >
                {Object.entries(modes).map(([value, label]) => (
                  <NativeSelectOption key={value} value={value}>
                    {label}
                  </NativeSelectOption>
                ))}
              </NativeSelect>
              {mutation.error && <p role="alert">{mutation.error}</p>}
              {mutation.status === 409 && (
                <Button
                  type="button"
                  variant="outline"
                  className="w-fit"
                  onClick={() => {
                    setDraft(undefined);
                    mutation.reset();
                    preference.retry();
                  }}
                >
                  Reload preference
                </Button>
              )}
              <Button
                type="submit"
                className="justify-self-end"
                disabled={
                  !draft ||
                  draft === saved.mode ||
                  mutation.pending ||
                  mutation.status === 409
                }
              >
                {mutation.pending ? "Saving…" : "Save preference"}
              </Button>
            </form>
          )}
        </Result>
      </DialogContent>
    </Dialog>
  );
}
