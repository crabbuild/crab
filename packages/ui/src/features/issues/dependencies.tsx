import { useState } from "react";
import { Link } from "@/components/navigation/link";
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
import { useMutation } from "@/components/discussion/mutations";
import { endpoint, useRequest, type Repository } from "@/lib/api";
import { repoHref } from "@/lib/repository-route";
import { WorkPicker } from "@/features/projects/item";
import type { Work } from "@/features/projects/model";

const directions = {
  blocked_by: { title: "Blocked by", add: "Add blocker" },
  blocking: { title: "Blocking", add: "Add blocked issue" },
} as const;
type Direction = keyof typeof directions;
type Page = {
  items: { work: Work; version: number }[];
  next: number | null;
  can_manage: boolean;
};
type Edge = { active: boolean; version: number };
type Props = {
  repo: Repository;
  csrf: string;
  issue: number;
  onChanged: () => void;
};

export function IssueDependencies(props: Props) {
  const [open, setOpen] = useState(false);
  return (
    <details
      className="linked-work"
      onToggle={(event) => setOpen(event.currentTarget.open)}
    >
      <summary className="cursor-pointer font-semibold">
        Issue dependencies
      </summary>
      {open && (
        <div className="grid gap-4 pt-3">
          <DependencyList {...props} direction="blocked_by" />
          <DependencyList {...props} direction="blocking" />
        </div>
      )}
    </details>
  );
}

function DependencyList({
  repo,
  csrf,
  issue,
  onChanged,
  direction,
}: Props & { direction: Direction }) {
  const labels = directions[direction];
  const path = `issues/${issue}/dependencies/${direction}`;
  const [after, setAfter] = useState<number>();
  const [adding, setAdding] = useState(false);
  const [picked, setPicked] = useState<Work>();
  const mutation = useMutation(csrf, repo);
  const resource = useRequest<Page>(
    endpoint(repo, path, { after: after?.toString() }),
    repo.id,
  );
  const selection = useRequest<Edge>(
    adding && picked
      ? endpoint(repo, `${path}/${picked.subject.number}`)
      : null,
    repo.id,
  );
  const canManage = Boolean(resource.data?.can_manage) && !repo.archived;
  const failure = mutation.error && (
    <div className="grid gap-2">
      <p role="alert" className="text-sm text-destructive">
        {mutation.error}
      </p>
      {mutation.conflict && (
        <Button
          type="button"
          variant="outline"
          size="sm"
          onClick={() => {
            resource.retry();
            selection.retry();
            mutation.reset();
          }}
        >
          Reload dependencies
        </Button>
      )}
    </div>
  );

  async function save(number: number, version: number, active: boolean) {
    if (!canManage || mutation.conflict) return;
    const saved = await mutation.run<Edge>(
      endpoint(repo, `${path}/${number}`),
      "PUT",
      { version, active },
    );
    if (saved) {
      setAdding(false);
      setPicked(undefined);
      resource.retry();
      onChanged();
    }
  }

  return (
    <section aria-label={labels.title} className="grid gap-2">
      <Dialog
        open={adding}
        onOpenChange={(value) => {
          if (mutation.pending) return;
          setAdding(value);
          setPicked(undefined);
          mutation.reset();
        }}
      >
        <div className="flex flex-wrap items-center justify-between gap-2">
          <strong>{labels.title}</strong>
          {canManage && (
            <DialogTrigger asChild>
              <Button variant="outline" size="sm">
                {labels.add}
              </Button>
            </DialogTrigger>
          )}
        </div>
        <Result state={resource} timingLabel={labels.title}>
          {(page) => (
            <>
              {page.items.length ? (
                <ul>
                  {page.items.map(({ work, version }) => (
                    <li key={work.subject.number}>
                      <Link
                        href={repoHref(repo, {
                          view: "issues",
                          issue: String(work.subject.number),
                        })}
                      >
                        #{work.subject.number} {work.title}
                      </Link>
                      <span className="muted">
                        {work.state === "closed" ? "Closed" : "Open"}
                      </span>
                      {canManage && (
                        <Button
                          type="button"
                          size="sm"
                          variant="ghost"
                          disabled={mutation.pending || mutation.conflict}
                          aria-label={`Remove dependency #${work.subject.number}`}
                          onClick={() =>
                            void save(work.subject.number, version, false)
                          }
                        >
                          Remove
                        </Button>
                      )}
                    </li>
                  ))}
                </ul>
              ) : (
                <p className="muted">No issues.</p>
              )}
              {(after !== undefined || page.next !== null) && (
                <div className="flex flex-wrap gap-2">
                  {after !== undefined && (
                    <Button
                      size="sm"
                      variant="outline"
                      onClick={() => setAfter(undefined)}
                    >
                      First page
                    </Button>
                  )}
                  {page.next !== null && (
                    <Button
                      size="sm"
                      variant="outline"
                      onClick={() => setAfter(page.next ?? undefined)}
                    >
                      More issues
                    </Button>
                  )}
                </div>
              )}
            </>
          )}
        </Result>
        {!adding && failure}
        <DialogContent showCloseButton={!mutation.pending}>
          <DialogHeader>
            <DialogTitle>{labels.add}</DialogTitle>
            <DialogDescription>
              {direction === "blocked_by"
                ? "Choose an issue that must be completed before this one."
                : "Choose an issue that depends on this one."}
            </DialogDescription>
          </DialogHeader>
          {adding && (
            <WorkPicker
              repo={repo}
              kind="issue"
              excludeIssue={issue}
              disabled={mutation.pending}
              onSelect={(work) => {
                setPicked(work);
                mutation.reset();
              }}
            />
          )}
          {picked && (
            <Result state={selection}>
              {(edge) => (
                <div className="grid gap-2">
                  <p>
                    #{picked.subject.number} {picked.title}
                  </p>
                  {edge.active ? (
                    <p>This issue is already linked here.</p>
                  ) : (
                    <Button
                      disabled={
                        !canManage || mutation.pending || mutation.conflict
                      }
                      onClick={() =>
                        void save(picked.subject.number, edge.version, true)
                      }
                    >
                      {mutation.pending ? "Adding…" : "Add dependency"}
                    </Button>
                  )}
                </div>
              )}
            </Result>
          )}
          {adding && failure}
        </DialogContent>
      </Dialog>
    </section>
  );
}
