import { useId, useState } from "react";
import { XIcon } from "lucide-react";
import { useMutation } from "@/components/discussion/mutations";
import { Result } from "@/components/request/result";
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
import { Input } from "@/components/ui/input";
import { TopicLinks } from "@/features/repository/topic-links";
import { useRequest, type Repository } from "@/lib/api";

type Topics = { names: string[]; version: number };
const limit = 20;

export function TopicSettings({
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
    <section className="settings-section" aria-labelledby="repository-topics">
      <header>
        <div className="min-w-0">
          <h3 id="repository-topics">Topics</h3>
          {repo.topics?.length ? (
            <TopicLinks names={repo.topics} />
          ) : (
            <p>No topics</p>
          )}
        </div>
        {!repo.archived && (
          <Dialog open={open} onOpenChange={setOpen}>
            <DialogTrigger asChild>
              <Button type="button" variant="outline" size="sm">
                Edit topics
              </Button>
            </DialogTrigger>
            <DialogContent>
              <DialogHeader>
                <DialogTitle>Edit repository topics</DialogTitle>
                <DialogDescription>
                  Add up to 20 topics. Use letters, numbers, and hyphens.
                </DialogDescription>
              </DialogHeader>
              {open && (
                <TopicForm
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

function TopicForm({
  repo,
  csrf,
  onSaved,
}: {
  repo: Repository;
  csrf: string;
  onSaved: () => void;
}) {
  const path = `/api/repositories/${repo.id}/topics`;
  const settings = useRequest<Topics>(path);
  const mutation = useMutation(csrf, repo);
  const [draft, setDraft] = useState<string[]>();
  const [input, setInput] = useState("");
  const inputId = useId();
  const stale = mutation.code === "repository_topics_changed";

  return (
    <Result state={settings} timingLabel="Repository topics">
      {(current) => {
        const names = draft ?? current.names;
        const topic = input.trim().toLowerCase();
        const error = !topic
          ? undefined
          : !/^[a-z0-9-]{1,50}$/.test(topic)
            ? "Use 1–50 letters, numbers, or hyphens."
            : names.includes(topic)
              ? "This topic is already included."
              : names.length >= limit
                ? "Remove a topic before adding another."
                : undefined;
        const canAdd = !!topic && !error;
        const submitted = canAdd ? [...names, topic].sort() : names;
        const changed =
          submitted.length !== current.names.length ||
          submitted.some((name, index) => name !== current.names[index]);
        function add() {
          if (!canAdd) return;
          setDraft(submitted);
          setInput("");
        }

        return (
          <form
            className="grid gap-3"
            onSubmit={async (event) => {
              event.preventDefault();
              if (error || stale || !changed || mutation.pending) return;
              const saved = await mutation.run<Topics>(path, "PUT", {
                names: submitted,
                version: current.version,
              });
              if (saved) onSaved();
            }}
          >
            <fieldset
              className="grid gap-2"
              disabled={mutation.pending || stale}
            >
              <legend className="sr-only">Repository topics</legend>
              {names.length > 0 && (
                <ul
                  className="flex flex-wrap gap-2"
                  aria-label="Selected topics"
                >
                  {names.map((name) => (
                    <li key={name} className="min-w-0">
                      <Badge
                        variant="secondary"
                        className="h-auto whitespace-normal break-all"
                      >
                        {name}
                        <Button
                          type="button"
                          variant="ghost"
                          size="icon-sm"
                          aria-label={`Remove topic ${name}`}
                          onClick={() =>
                            setDraft(names.filter((topic) => topic !== name))
                          }
                        >
                          <XIcon />
                        </Button>
                      </Badge>
                    </li>
                  ))}
                </ul>
              )}
              <label htmlFor={inputId} className="text-sm font-medium">
                New topic
              </label>
              <div className="flex gap-2">
                <Input
                  id={inputId}
                  value={input}
                  autoComplete="off"
                  aria-describedby={error ? `${inputId}-error` : undefined}
                  onChange={(event) => setInput(event.target.value)}
                  onKeyDown={(event) => {
                    if (event.key !== "Enter" || event.nativeEvent.isComposing)
                      return;
                    event.preventDefault();
                    add();
                  }}
                />
                <Button
                  type="button"
                  variant="outline"
                  disabled={!canAdd}
                  onClick={add}
                >
                  Add topic
                </Button>
              </div>
              {error && (
                <p id={`${inputId}-error`} role="status">
                  {error}
                </p>
              )}
            </fieldset>
            {mutation.error && <p role="alert">{mutation.error}</p>}
            {stale && (
              <Button
                type="button"
                variant="outline"
                className="w-fit"
                onClick={() => {
                  setDraft(undefined);
                  setInput("");
                  mutation.reset();
                  settings.retry();
                }}
              >
                Reload topics
              </Button>
            )}
            <Button
              type="submit"
              className="justify-self-end"
              disabled={!changed || !!error || stale || mutation.pending}
            >
              {mutation.pending ? "Saving…" : "Save topics"}
            </Button>
          </form>
        );
      }}
    </Result>
  );
}
