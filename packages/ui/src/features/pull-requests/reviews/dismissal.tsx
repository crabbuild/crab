import { useId, useRef, useState } from "react";
import { MoreHorizontalIcon } from "lucide-react";
import type { Repository } from "@/lib/api";
import { endpoint, useRequest } from "@/lib/api";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuGroup,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import {
  Field,
  FieldError,
  FieldGroup,
  FieldLabel,
} from "@/components/ui/field";
import { Textarea } from "@/components/ui/textarea";
import { Result } from "@/components/request/result";
import { DiscussionMarkdown } from "@/components/discussion/markdown";
import { useMutation } from "@/components/discussion/mutations";
import type { Review } from "@/components/discussion/conversation/model";

type Intent = { request_id: string; version: number; body: string };
export function ReviewDismissal({
  repo,
  target,
  review,
  csrf,
  onSaved,
}: {
  repo: Repository;
  target: string;
  review: Review;
  csrf: string;
  onSaved: () => void;
}) {
  const [open, setOpen] = useState(false);
  const [body, setBody] = useState("");
  const [snapshot, setSnapshot] = useState(review);
  const [intent, setIntent] = useState<Intent>();
  const [inspect, setInspect] = useState(false);
  const mutation = useMutation(csrf, repo);
  const trigger = useRef<HTMLButtonElement>(null);
  const completed = useRef(false);
  const id = useId();
  const url = endpoint(repo, `${target}/reviews/${review.number}`);
  const latest = useRequest<Review>(inspect ? url : null);
  const allowed = snapshot.can_dismiss && !repo.archived;
  const valid =
    body.trim().length > 0 && new TextEncoder().encode(body).length <= 65536;
  if (!review.can_dismiss && !intent && !open) return null;
  async function submit() {
    const original = intent ?? {
      request_id: crypto.randomUUID(),
      version: snapshot.version,
      body,
    };
    setIntent(original);
    const saved = await mutation.run<{ dismissed: true }>(
      endpoint(repo, `${target}/reviews/${review.number}/dismissals`),
      "PUT",
      original,
    );
    if (saved) {
      completed.current = true;
      setIntent(undefined);
      setBody("");
      setOpen(false);
      onSaved();
    }
  }
  // A rejected attempt may be revised. An uncertain reply retains its exact
  // version, UUID and reason, even if write permission disappears afterward.
  const recovering = Boolean(intent && mutation.error && !mutation.rejected);
  return (
    <>
      <DropdownMenu>
        <DropdownMenuTrigger asChild>
          <Button
            ref={trigger}
            variant="ghost"
            size="icon-sm"
            aria-label={`Review #${review.number} actions`}
            disabled={mutation.pending}
          >
            <MoreHorizontalIcon />
          </Button>
        </DropdownMenuTrigger>
        <DropdownMenuContent
          align="end"
          onCloseAutoFocus={(event) => {
            if (open) event.preventDefault();
          }}
        >
          <DropdownMenuGroup>
            <DropdownMenuItem
              onSelect={() => {
                if (!intent) {
                  setSnapshot(review);
                  mutation.reset();
                  setInspect(false);
                }
                completed.current = false;
                setOpen(true);
              }}
            >
              {intent ? "Recover review dismissal" : "Dismiss review"}
            </DropdownMenuItem>
          </DropdownMenuGroup>
        </DropdownMenuContent>
      </DropdownMenu>
      <Dialog
        open={open}
        onOpenChange={(value) => {
          if (!mutation.pending) setOpen(value);
        }}
      >
        {open && (
          <DialogContent
            className="max-h-[calc(100svh-2rem)] overflow-y-auto sm:max-w-xl"
            onCloseAutoFocus={(event) => {
              event.preventDefault();
              if (completed.current)
                document
                  .getElementById(`pullrequestreview-${review.number}`)
                  ?.focus();
              else trigger.current?.focus();
            }}
          >
            <DialogHeader>
              <DialogTitle>Dismiss review</DialogTitle>
              <DialogDescription>
                Explain why this review should no longer count toward approval
                or requested changes. The reason will appear in the
                conversation.
              </DialogDescription>
            </DialogHeader>
            <form
              onSubmit={(event) => {
                event.preventDefault();
                void submit();
              }}
            >
              <FieldGroup>
                <Field>
                  <FieldLabel htmlFor={id}>Reason for dismissal</FieldLabel>
                  <Textarea
                    id={id}
                    value={body}
                    onChange={(event) => setBody(event.target.value)}
                    required
                    disabled={mutation.pending || recovering}
                    rows={4}
                  />
                </Field>
                {mutation.error && <FieldError>{mutation.error}</FieldError>}
                {recovering && (
                  <p className="text-sm text-muted-foreground" role="status">
                    The original dismissal may have completed. Retry it to
                    recover the result.
                  </p>
                )}
                {mutation.rejected && intent && (
                  <Button
                    type="button"
                    variant="outline"
                    onClick={() => {
                      setIntent(undefined);
                      mutation.reset();
                    }}
                  >
                    Revise dismissal
                  </Button>
                )}
                {mutation.conflict && (
                  <Button
                    type="button"
                    variant="outline"
                    onClick={() => setInspect(true)}
                  >
                    Review current version
                  </Button>
                )}
                {inspect && (
                  <Result
                    state={latest}
                    timingLabel="Review dismissal conflict"
                  >
                    {(current) => (
                      <div className="flex flex-col gap-3">
                        <p className="text-sm text-muted-foreground">
                          Current review · version {current.version} ·{" "}
                          {current.state.replaceAll("_", " ")}
                        </p>
                        <DiscussionMarkdown>{current.body}</DiscussionMarkdown>
                        {current.can_dismiss && !repo.archived ? (
                          <Button
                            type="button"
                            variant="outline"
                            onClick={() => {
                              setSnapshot(current);
                              setIntent(undefined);
                              setInspect(false);
                              mutation.reset();
                            }}
                          >
                            Continue with current review
                          </Button>
                        ) : (
                          <p role="status">
                            This review can no longer be dismissed. Your reason
                            remains available to copy.
                          </p>
                        )}
                      </div>
                    )}
                  </Result>
                )}
                <DialogFooter>
                  <Button
                    type="button"
                    variant="outline"
                    disabled={mutation.pending}
                    onClick={() => setOpen(false)}
                  >
                    Cancel
                  </Button>
                  <Button
                    type="submit"
                    variant="destructive"
                    disabled={
                      mutation.pending ||
                      inspect ||
                      (intent ? mutation.rejected : !allowed || !valid)
                    }
                  >
                    {mutation.pending
                      ? "Dismissing…"
                      : intent
                        ? "Retry original dismissal"
                        : "Dismiss review"}
                  </Button>
                </DialogFooter>
              </FieldGroup>
            </form>
          </DialogContent>
        )}
      </Dialog>
    </>
  );
}
