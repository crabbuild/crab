import { useState } from "react";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";
import { useOverlayState } from "@/components/ui/use-overlay-state";
import { Result } from "@/components/request/result";
import { endpoint, useRequest, type Repository } from "@/lib/api";
import { Author, type DiscussionAuthor } from "../author";
import { Failure } from "../failure";
import { useMutation } from "../mutations";
import { timestamp } from "../time";

interface Revision {
  number: number;
  editor: DiscussionAuthor;
  edited_at: number;
  redaction: { author: DiscussionAuthor; at: number } | null;
}

interface Content {
  body: string;
  suggestion: string | null;
}

interface HistoryPage {
  items: Revision[];
  next: number | null;
}

interface RevisionDetail {
  revision: Revision;
  before: Content | null;
  after: Content | null;
  can_redact: boolean;
}

export function EditHistory({
  repo,
  csrf,
  target,
}: {
  repo: Repository;
  csrf: string;
  target: string;
}) {
  const mutation = useMutation(csrf, repo);
  const [open, setOpen] = useState(false);
  const overlay = useOverlayState({
    open,
    onOpenChange: (value) => {
      if (!mutation.pending) {
        mutation.reset();
        setOpen(value);
      }
    },
  });

  return (
    <Dialog {...overlay}>
      <DialogTrigger asChild>
        <Button
          type="button"
          variant="ghost"
          size="sm"
          className="h-auto px-1 py-0.5 text-xs text-muted-foreground"
          aria-label="View edit history"
        >
          edited
        </Button>
      </DialogTrigger>
      <DialogContent
        className="max-h-[85dvh] overflow-y-auto sm:max-w-4xl"
        showCloseButton={!mutation.pending}
      >
        <DialogHeader>
          <DialogTitle>Edit history</DialogTitle>
          <DialogDescription>
            Select a revision to compare the text before and after the edit.
          </DialogDescription>
        </DialogHeader>
        {overlay.open && (
          <History repo={repo} target={target} mutation={mutation} />
        )}
      </DialogContent>
    </Dialog>
  );
}

function History({
  repo,
  target,
  mutation,
}: {
  repo: Repository;
  target: string;
  mutation: ReturnType<typeof useMutation>;
}) {
  const [before, setBefore] = useState<number>();
  const [selected, setSelected] = useState<number>();
  const [confirm, setConfirm] = useState<number>();
  const [removed, setRemoved] = useState<Set<number>>(() => new Set());
  const endpointPath = `edit-history/${target}`;
  const list = useRequest<HistoryPage>(
    endpoint(repo, endpointPath, { before: before?.toString() }),
    repo.id,
  );
  const revision = selected ?? list.data?.items[0]?.number;
  const revisionUrl =
    revision === undefined
      ? null
      : endpoint(repo, endpointPath, { revision: revision.toString() });
  const detail = useRequest<RevisionDetail>(revisionUrl, repo.id);

  function select(number?: number) {
    setSelected(number);
    setConfirm(undefined);
    mutation.reset();
  }

  async function redact() {
    if (!revisionUrl || revision === undefined || confirm !== revision) return;
    const saved = await mutation.run<{ redacted: boolean }>(
      revisionUrl,
      "DELETE",
      {},
    );
    if (!saved?.redacted) return;
    // Remove the previously fetched text before requesting the tombstone.
    setRemoved((entries) => new Set(entries).add(revision));
    setConfirm(undefined);
    list.retry();
    detail.retry();
  }

  return (
    <Result state={list}>
      {(page) =>
        page.items.length ? (
          <div className="grid min-w-0 gap-4 md:grid-cols-[12rem_minmax(0,1fr)]">
            <div
              aria-label="Revisions"
              role="group"
              className="grid content-start gap-1"
            >
              {page.items.map((edit) => (
                <Button
                  key={edit.number}
                  type="button"
                  variant={edit.number === revision ? "secondary" : "ghost"}
                  aria-pressed={edit.number === revision}
                  className="h-auto flex-col items-start gap-0 px-2 py-2 text-left whitespace-normal"
                  disabled={mutation.pending}
                  onClick={() => select(edit.number)}
                >
                  <span>{edit.editor.profile?.name ?? edit.editor.name}</span>
                  <time
                    className="text-xs text-muted-foreground"
                    dateTime={new Date(edit.edited_at).toISOString()}
                  >
                    {timestamp(edit.edited_at)}
                  </time>
                  {(edit.redaction || removed.has(edit.number)) && (
                    <span className="text-xs">Content removed</span>
                  )}
                </Button>
              ))}
              {(before !== undefined || page.next !== null) && (
                <div className="flex flex-wrap gap-1">
                  {before !== undefined && (
                    <Button
                      type="button"
                      variant="outline"
                      size="sm"
                      disabled={mutation.pending}
                      onClick={() => {
                        select();
                        setBefore(undefined);
                      }}
                    >
                      Latest
                    </Button>
                  )}
                  {page.next !== null && (
                    <Button
                      type="button"
                      variant="outline"
                      size="sm"
                      disabled={mutation.pending}
                      onClick={() => {
                        select();
                        setBefore(page.next ?? undefined);
                      }}
                    >
                      Older edits
                    </Button>
                  )}
                </div>
              )}
            </div>
            <Result state={detail}>
              {({ revision: edit, before: previous, after, can_redact }) => (
                <section
                  aria-label="Revision details"
                  className="grid min-w-0 content-start gap-3"
                >
                  <div className="flex flex-wrap items-center justify-between gap-2">
                    <p className="text-muted-foreground">
                      Edited by <Author author={edit.editor} /> ·{" "}
                      {timestamp(edit.edited_at)}
                    </p>
                    {can_redact &&
                      !edit.redaction &&
                      !removed.has(edit.number) &&
                      confirm !== edit.number && (
                        <Button
                          type="button"
                          variant="outline"
                          size="sm"
                          onClick={() => setConfirm(edit.number)}
                        >
                          Remove revision
                        </Button>
                      )}
                  </div>
                  {confirm === edit.number && (
                    <div
                      role="group"
                      aria-label="Confirm removal"
                      className="grid gap-2 rounded border p-3"
                    >
                      <p>
                        Remove this revision’s text from the history? This
                        action is permanent.
                      </p>
                      <Failure message={mutation.error} />
                      <div className="flex flex-wrap gap-2">
                        <Button
                          type="button"
                          variant="destructive"
                          disabled={mutation.pending}
                          onClick={() => void redact()}
                        >
                          {mutation.pending ? "Removing…" : "Confirm removal"}
                        </Button>
                        <Button
                          type="button"
                          variant="outline"
                          disabled={mutation.pending}
                          onClick={() => {
                            setConfirm(undefined);
                            mutation.reset();
                          }}
                        >
                          Cancel
                        </Button>
                      </div>
                    </div>
                  )}
                  {edit.redaction || removed.has(edit.number) ? (
                    <p role="status">
                      Revision content removed
                      {edit.redaction && (
                        <>
                          {" "}
                          by <Author author={edit.redaction.author} /> ·{" "}
                          {timestamp(edit.redaction.at)}
                        </>
                      )}
                    </p>
                  ) : (
                    <div className="grid min-w-0 gap-3 md:grid-cols-2">
                      <Version title="Before this edit" content={previous} />
                      <Version title="After this edit" content={after} />
                    </div>
                  )}
                </section>
              )}
            </Result>
          </div>
        ) : (
          <p>No recorded edits.</p>
        )
      }
    </Result>
  );
}

function Version({
  title,
  content,
}: {
  title: string;
  content: Content | null;
}) {
  return (
    <section className="grid min-w-0 content-start gap-2 rounded border p-3">
      <h3 className="font-medium">{title}</h3>
      {content ? (
        <>
          <pre className="max-h-80 overflow-auto whitespace-pre-wrap wrap-anywhere">
            {content.body || "Empty body"}
          </pre>
          {content.suggestion !== null && (
            <>
              <h4>Suggested replacement</h4>
              <pre className="max-h-80 overflow-auto whitespace-pre-wrap wrap-anywhere">
                {content.suggestion || "Remove selected lines"}
              </pre>
            </>
          )}
        </>
      ) : (
        <p className="text-muted-foreground">Version unavailable</p>
      )}
    </section>
  );
}
