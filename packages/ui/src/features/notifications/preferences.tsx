import { BellIcon } from "lucide-react";
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
import {
  NativeSelect,
  NativeSelectOption,
} from "@/components/ui/native-select";
import { refreshNotificationBadge } from "./index";
import { endpoint, useRequest, type Repository } from "@/lib/api";

const repositoryModes = {
  direct: "Direct notifications",
  issues: "Issue updates",
  pull_requests: "Pull request updates",
  issues_and_pull_requests: "Issue and pull request updates",
  muted: "Muted",
};

const discussionModes = {
  inherit: "Use repository setting",
  follow: "All discussion activity",
  state_changes: "State changes only",
  unsubscribed: "Unsubscribe from updates",
};

type Preference = { mode: string; version: number };

export function RepositoryWatch({
  repo,
  csrf,
}: {
  repo: Repository;
  csrf: string;
}) {
  return (
    <NotificationPreference
      repo={repo}
      csrf={csrf}
      path={endpoint(repo, "subscription")}
      modes={repositoryModes}
      label="Watch"
      title="Repository notifications"
      description={`Choose what appears in your inbox for ${repo.owner}/${repo.name}. Direct notifications include assignments, review requests, and mentions.`}
    />
  );
}

export function DiscussionWatch({
  repo,
  csrf,
  subject,
}: {
  repo: Repository;
  csrf: string;
  subject: string;
}) {
  const title = subject.startsWith("issues/")
    ? "Issue notifications"
    : "Pull request notifications";
  return (
    <section>
      <header>
        <strong>Notifications</strong>
      </header>
      <NotificationPreference
        repo={repo}
        csrf={csrf}
        path={endpoint(repo, `${subject}/subscription`)}
        modes={discussionModes}
        label="Notification settings"
        title={title}
        description="Creating an issue or pull request, posting a comment or review, or changing its state follows future activity. Repository mute still applies."
      />
    </section>
  );
}

function NotificationPreference({
  repo,
  csrf,
  path,
  modes,
  label,
  title,
  description,
}: {
  repo: Repository;
  csrf: string;
  path: string;
  modes: Record<string, string>;
  label: string;
  title: string;
  description: string;
}) {
  const modeId = useId();
  const [open, setOpen] = useState(false);
  const [draft, setDraft] = useState<string>();
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
          {label}
        </Button>
      </DialogTrigger>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>{title}</DialogTitle>
          <DialogDescription>{description}</DialogDescription>
        </DialogHeader>
        <Result state={preference} timingLabel={title}>
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
              <label className="text-sm font-medium" htmlFor={modeId}>
                Notify me about
              </label>
              <NativeSelect
                id={modeId}
                value={draft ?? saved.mode}
                disabled={mutation.pending}
                onChange={(event) => setDraft(event.target.value)}
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
