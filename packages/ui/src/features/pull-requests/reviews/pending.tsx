import { useState } from "react";
import { Editor } from "@/components/discussion/editor";
import { useSubmission } from "@/components/discussion/hooks";
import { useMutation } from "@/components/discussion/mutations";
import { Button } from "@/components/ui/button";
import { Field, FieldGroup, FieldLabel } from "@/components/ui/field";
import { Textarea } from "@/components/ui/textarea";
import { endpoint, useRequest, type Repository } from "@/lib/api";

interface PendingAnchor {
  path_hex: string;
  base_oid: string;
  head_oid: string;
  side: "old" | "new";
  start_line: number;
  end_line: number;
  body: string;
  suggested_text: string | null;
}

interface PendingComment extends PendingAnchor {
  id: string;
  path: string;
}

interface PendingDraft {
  version: string | null;
  comments: PendingComment[];
}

type Edit =
  | {
      action: "save";
      comment: PendingAnchor & { request_id: string };
      replace?: string;
    }
  | { action: "remove"; id: string }
  | { action: "discard" };

export function usePendingReview(
  repo: Repository,
  number: number,
  csrf: string,
  active: boolean,
) {
  const url = endpoint(repo, `pulls/${number}/pending-review`);
  const request = useRequest<PendingDraft>(active ? url : null);
  const mutation = useMutation(csrf, repo);
  const [saved, setSaved] = useState<{
    draft: PendingDraft;
    from: PendingDraft;
  }>();
  const draft =
    saved && saved.from === request.data ? saved.draft : request.data;

  function reload() {
    setSaved(undefined);
    mutation.reset();
    request.retry();
  }
  async function change(edit: Edit, requestId: string) {
    if (
      !request.data ||
      !draft ||
      request.loading ||
      request.error ||
      mutation.conflict
    )
      return false;
    const next = await mutation.run<PendingDraft>(url, "PUT", {
      expected: draft.version,
      request_id: requestId,
      edit,
    });
    if (next) setSaved({ draft: next, from: request.data });
    return !!next;
  }
  return {
    draft,
    ready: !!draft && !request.loading && !request.error && !mutation.conflict,
    error: request.error ?? mutation.error,
    pending: mutation.pending,
    change,
    reload,
  };
}

export type PendingReview = ReturnType<typeof usePendingReview>;

export function PendingComments({
  review,
  canEdit,
  canDiscard,
  busy,
  head,
}: {
  review: PendingReview;
  canEdit: boolean;
  canDiscard: boolean;
  busy: boolean;
  head: string;
}) {
  const [edit, setEdit] = useState<PendingComment>();
  const [confirmDiscard, setConfirmDiscard] = useState(false);
  const submission = useSubmission();
  const comments = review.draft?.comments;
  const disabled = !review.ready || review.pending || busy;
  return (
    <>
      {review.error && (
        <div className="discussion-actions">
          <p role="alert">{review.error}</p>
          <Button
            type="button"
            size="sm"
            variant="outline"
            disabled={busy}
            onClick={review.reload}
          >
            Reload pending review
          </Button>
        </div>
      )}
      {!!comments?.length && (
        <section
          className="panel discussion-form"
          aria-label="Pending review comments"
        >
          <h3>Pending review comments ({comments.length})</h3>
          <p className="muted">
            Only you can see these comments until you submit your review.
          </p>
          {comments[0].head_oid !== head && (
            <p role="status">
              These comments target an earlier commit. Copy the comments you
              want to keep before discarding the draft.
            </p>
          )}
          <ul className="grid gap-4">
            {comments.map((comment) => (
              <li key={comment.id} className="grid gap-2">
                <p>
                  <code>{comment.path}</code> ·{" "}
                  {comment.side === "old" ? "Old" : "New"}{" "}
                  {comment.start_line === comment.end_line
                    ? `line ${comment.start_line}`
                    : `lines ${comment.start_line}–${comment.end_line}`}
                </p>
                {edit?.id === comment.id ? (
                  <form
                    className="grid gap-2"
                    onSubmit={async (event) => {
                      event.preventDefault();
                      const { body, suggested_text } = edit;
                      const {
                        base_oid,
                        head_oid,
                        path_hex,
                        side,
                        start_line,
                        end_line,
                      } = comment;
                      const request_id = submission({
                        id: comment.id,
                        body,
                        suggested_text,
                        version: review.draft?.version,
                      });
                      if (
                        await review.change(
                          {
                            action: "save",
                            replace: comment.id,
                            comment: {
                              body,
                              suggested_text,
                              base_oid,
                              head_oid,
                              path_hex,
                              side,
                              start_line,
                              end_line,
                              request_id,
                            },
                          },
                          request_id,
                        )
                      )
                        setEdit(undefined);
                    }}
                  >
                    <FieldGroup>
                      <Editor
                        id="pending-review-comment"
                        label="Pending comment"
                        value={edit.body}
                        onChange={(body) => setEdit({ ...edit, body })}
                        disabled={disabled}
                        required
                      />
                      {comment.side === "new" && (
                        <Field>
                          <FieldLabel htmlFor="pending-review-suggestion">
                            Suggested replacement
                          </FieldLabel>
                          <Textarea
                            id="pending-review-suggestion"
                            value={edit.suggested_text ?? ""}
                            rows={3}
                            maxLength={65536}
                            disabled={disabled}
                            onChange={(event) =>
                              setEdit({
                                ...edit,
                                suggested_text: event.target.value,
                              })
                            }
                          />
                          {edit.suggested_text !== null && (
                            <Button
                              type="button"
                              size="sm"
                              variant="ghost"
                              className="justify-self-start"
                              disabled={disabled}
                              onClick={() =>
                                setEdit({ ...edit, suggested_text: null })
                              }
                            >
                              Remove suggestion
                            </Button>
                          )}
                        </Field>
                      )}
                    </FieldGroup>
                    <div className="discussion-actions">
                      <Button
                        size="sm"
                        type="submit"
                        disabled={disabled || !edit.body.trim()}
                      >
                        Save comment
                      </Button>
                      <Button
                        size="sm"
                        type="button"
                        variant="outline"
                        onClick={() => setEdit(undefined)}
                      >
                        Cancel
                      </Button>
                    </div>
                  </form>
                ) : (
                  <>
                    <p className="whitespace-pre-wrap break-words">
                      {comment.body}
                    </p>
                    {comment.suggested_text !== null && (
                      <pre className="overflow-auto rounded-md bg-muted p-2">
                        {comment.suggested_text || "Remove selected lines"}
                      </pre>
                    )}
                    {canDiscard && (
                      <div className="discussion-actions">
                        {canEdit && comment.head_oid === head && (
                          <Button
                            type="button"
                            size="sm"
                            variant="outline"
                            disabled={disabled}
                            onClick={() => setEdit(comment)}
                          >
                            Edit
                          </Button>
                        )}
                        <Button
                          type="button"
                          size="sm"
                          variant="outline"
                          disabled={disabled}
                          onClick={() =>
                            void review.change(
                              { action: "remove", id: comment.id },
                              crypto.randomUUID(),
                            )
                          }
                        >
                          Remove
                        </Button>
                      </div>
                    )}
                  </>
                )}
              </li>
            ))}
          </ul>
          {canDiscard && (
            <div className="discussion-actions">
              {confirmDiscard ? (
                <>
                  <p>Discard all pending inline comments?</p>
                  <Button
                    type="button"
                    size="sm"
                    variant="destructive"
                    disabled={disabled}
                    onClick={async () => {
                      if (
                        await review.change(
                          { action: "discard" },
                          crypto.randomUUID(),
                        )
                      ) {
                        setConfirmDiscard(false);
                        setEdit(undefined);
                      }
                    }}
                  >
                    Discard comments
                  </Button>
                  <Button
                    type="button"
                    size="sm"
                    variant="outline"
                    onClick={() => setConfirmDiscard(false)}
                  >
                    Keep editing
                  </Button>
                </>
              ) : (
                <Button
                  type="button"
                  size="sm"
                  variant="outline"
                  disabled={disabled}
                  onClick={() => setConfirmDiscard(true)}
                >
                  Discard pending review
                </Button>
              )}
            </div>
          )}
        </section>
      )}
    </>
  );
}
