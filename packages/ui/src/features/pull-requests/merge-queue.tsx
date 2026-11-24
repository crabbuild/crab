import { FieldSelect } from "@/components/ui/field-select";
import { useState } from "react";
import { Link } from "@/components/navigation/link";
import { Failure } from "@/components/discussion/failure";
import { useMutation } from "@/components/discussion/mutations";
import { Result } from "@/components/request/result";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import {
  endpoint,
  navigate,
  useRequest,
  type Refs,
  type Repository,
} from "@/lib/api";
import type {
  MergeQueueItem,
  MergeQueuePage as QueuePage,
  MergeQueueState,
} from "@/lib/contracts";
import { short } from "@/lib/format";
import { repoHref } from "@/lib/repository-route";

const stateLabels: Record<MergeQueueState, string> = {
  waiting: "Waiting",
  preparing: "Preparing candidate",
  candidate_scheduled: "Candidate scheduled",
  publishing: "Publishing",
};

export function MergeQueue({
  repo,
  refs,
  url,
  csrf,
}: {
  repo: Repository;
  refs: Refs;
  url: URL;
  csrf: string;
}) {
  const branches = refs.refs.filter((ref) =>
    ref.name.startsWith("refs/heads/"),
  );
  const branch =
    url.searchParams.get("branch") ||
    branches.find((ref) => ref.name === refs.head?.name)?.name ||
    branches[0]?.name;
  const before = url.searchParams.get("before") ?? undefined;
  return (
    <section className="grid gap-4" aria-labelledby="merge-queue-heading">
      <Link className="w-fit text-sm" href={repoHref(repo, { view: "pulls" })}>
        ← Pull requests
      </Link>
      <div className="flex flex-wrap items-end justify-between gap-3">
        <h2 className="text-2xl font-semibold" id="merge-queue-heading">
          Merge queue
        </h2>
        {branch && (
          <div className="grid gap-1.5">
            <Label htmlFor="merge-queue-branch">Target branch</Label>
            <FieldSelect
              id="merge-queue-branch"
              value={branch}
              onValueChange={(nextValue) =>
                navigate(
                  repoHref(repo, {
                    view: "pulls",
                    action: "queue",
                    branch: nextValue,
                  }),
                )
              }
              options={[
                ...(!branches.some((ref) => ref.name === branch)
                  ? [
                      {
                        value: branch,
                        label: branch.replace(/^refs\/heads\//, ""),
                      },
                    ]
                  : []),
                ...branches.map((ref) => ({
                  value: ref.name,
                  label: ref.name.slice("refs/heads/".length),
                })),
              ]}
            />
          </div>
        )}
      </div>
      {branch ? (
        <Entries
          key={branch}
          repo={repo}
          branch={branch}
          before={before}
          csrf={csrf}
        />
      ) : (
        <p className="notice">This repository has no branches yet.</p>
      )}
    </section>
  );
}

function Entries({
  repo,
  branch,
  before,
  csrf,
}: {
  repo: Repository;
  branch: string;
  before: string | undefined;
  csrf: string;
}) {
  const page = useRequest<QueuePage>(
    endpoint(repo, "pulls/queue", { branch, before }),
    repo.id,
  );
  const change = useMutation(csrf, repo);
  const [announcement, setAnnouncement] = useState("");
  const [priority, setPriority] = useState<{
    item: MergeQueueItem;
    requestId: string;
  }>();
  async function moveToFront() {
    if (!priority) return;
    const { item, requestId } = priority;
    const result = await change.run<{ request_id: string }>(
      endpoint(repo, `pulls/${item.pull}/merge-queue/priority`),
      "PUT",
      { request_id: requestId, queue_id: item.request_id },
    );
    if (result?.request_id !== requestId) return;
    setPriority(undefined);
    setAnnouncement(`Saved priority for pull request #${item.pull}.`);
    if (before) {
      navigate(repoHref(repo, { view: "pulls", action: "queue", branch }));
    } else {
      page.retry();
    }
  }

  async function remove(item: MergeQueueItem) {
    setAnnouncement("");
    const result = await change.run<{
      request: { stopped: string | null } | null;
    }>(endpoint(repo, `pulls/${item.pull}/auto-merge`), "DELETE", {
      request_id: item.request_id,
    });
    if (!result) return;
    if (result.request?.stopped === "canceled") {
      setAnnouncement(
        `Removed pull request #${item.pull} from the merge queue.`,
      );
    }
    page.retry();
  }

  return (
    <div className="grid gap-3">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <p className="text-sm text-muted-foreground">
          Entries are listed in merge order.
        </p>
        <Button
          variant="outline"
          size="sm"
          onClick={page.retry}
          disabled={page.loading || change.pending}
        >
          Refresh queue
        </Button>
      </div>
      {announcement && (
        <p role="status" className="text-sm">
          {announcement}
        </p>
      )}
      {!priority && <Failure message={change.error} />}
      <Dialog
        open={priority !== undefined}
        onOpenChange={(open) => {
          if (!open && !change.pending) {
            change.reset();
            setPriority(undefined);
          }
        }}
      >
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Move pull request to the front?</DialogTitle>
            <DialogDescription>
              The queue will rebuild candidates for{" "}
              {branch.replace(/^refs\/heads\//, "")} and run their required
              checks again.
            </DialogDescription>
          </DialogHeader>
          <Failure message={change.error} />
          <DialogFooter>
            <Button
              variant="outline"
              disabled={change.pending}
              onClick={() => {
                change.reset();
                setPriority(undefined);
              }}
            >
              Cancel
            </Button>
            <Button
              disabled={change.pending}
              onClick={() => void moveToFront()}
            >
              Confirm move to front
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
      <Result state={page} timingLabel="Merge queue">
        {(data) => (
          <>
            {data.items.length ? (
              <ol
                className="divide-y rounded-lg border"
                aria-label="Queued pull requests"
              >
                {data.items.map((item) => (
                  <li
                    key={item.request_id}
                    className="flex flex-wrap items-center justify-between gap-3 p-3"
                  >
                    <div className="grid min-w-0 gap-1">
                      <Link
                        className="font-medium wrap-anywhere"
                        href={repoHref(repo, {
                          view: "pulls",
                          pull: String(item.pull),
                        })}
                      >
                        #{item.pull} {item.title}
                      </Link>
                      <div className="flex flex-wrap items-center gap-2 text-sm">
                        <Badge variant="outline">
                          {stateLabels[item.state]}
                        </Badge>
                        <code
                          className="text-muted-foreground"
                          title={item.head_oid}
                        >
                          {short(item.head_oid)}
                        </code>
                        {(item.state === "candidate_scheduled" ||
                          item.state === "publishing") && (
                          <Link
                            href={repoHref(repo, {
                              view: "pulls",
                              pull: String(item.pull),
                              pull_tab: "checks",
                              candidate: "1",
                            })}
                          >
                            Candidate checks
                          </Link>
                        )}
                      </div>
                    </div>
                    <div className="flex flex-wrap gap-2">
                      {item.can_prioritize && (
                        <Button
                          variant="outline"
                          size="sm"
                          disabled={change.pending}
                          aria-label={`Move pull request #${item.pull} to front`}
                          onClick={() => {
                            change.reset();
                            setAnnouncement("");
                            setPriority({
                              item,
                              requestId: crypto.randomUUID(),
                            });
                          }}
                        >
                          Move to front
                        </Button>
                      )}
                      {item.can_remove && (
                        <Button
                          variant="outline"
                          size="sm"
                          disabled={change.pending}
                          aria-label={`Remove pull request #${item.pull} from queue`}
                          onClick={() => void remove(item)}
                        >
                          Remove from queue
                        </Button>
                      )}
                    </div>
                  </li>
                ))}
              </ol>
            ) : (
              <p className="notice">
                {before
                  ? "No more entries."
                  : "No pull requests are queued for this branch."}
              </p>
            )}
            {(before || data.next) && (
              <nav
                aria-label="Merge queue pages"
                className="flex flex-wrap gap-2"
              >
                {before && (
                  <Button variant="outline" asChild>
                    <Link
                      href={repoHref(repo, {
                        view: "pulls",
                        action: "queue",
                        branch,
                      })}
                    >
                      First page
                    </Link>
                  </Button>
                )}
                {data.next && (
                  <Button variant="outline" asChild>
                    <Link
                      href={repoHref(repo, {
                        view: "pulls",
                        action: "queue",
                        branch,
                        before: data.next,
                      })}
                    >
                      Next page
                    </Link>
                  </Button>
                )}
              </nav>
            )}
          </>
        )}
      </Result>
    </div>
  );
}
