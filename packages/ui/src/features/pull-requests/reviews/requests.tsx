import { useId, useRef, useState } from "react";
import { ClockIcon, CodeReviewIcon } from "@primer/octicons-react";
import { RotateCwIcon, SettingsIcon, XIcon } from "lucide-react";
import { Author, type DiscussionAuthor } from "@/components/discussion/author";
import { Button } from "@/components/ui/button";
import { Field, FieldError, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import {
  Popover,
  PopoverContent,
  PopoverHeader,
  PopoverTitle,
  PopoverTrigger,
} from "@/components/ui/popover";
import {
  endpoint,
  useRequest,
  type Repository,
  type RepositoryAssignee,
} from "@/lib/api";
import { useMutation } from "@/components/discussion/mutations";
import { Result } from "@/components/request/result";

interface ReviewRequest {
  id: string;
  reviewer: DiscussionAuthor;
  candidate_id: string | null;
  requested_by: DiscussionAuthor;
  state:
    | { kind: "requested" | "cancelled" }
    | { kind: "fulfilled"; review: number };
}
interface Page {
  items: ReviewRequest[];
  next: string | null;
  version: number;
  can_manage: boolean;
  candidates: RepositoryAssignee[];
}
type Change =
  | { kind: "request"; actor_id: string }
  | { kind: "cancel"; id: string };

export function ReviewRequests({
  repo,
  pull,
  csrf,
  onSaved,
}: {
  repo: Repository;
  pull: number;
  csrf: string;
  onSaved: () => void;
}) {
  const [before, setBefore] = useState<string>();
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [selected, setSelected] = useState<string>();
  const searchId = useId();
  const search = useRef<HTMLInputElement>(null);
  const resource = useRequest<Page>(
    endpoint(repo, `pulls/${pull}/review-requests`, { before }),
  );
  const mutation = useMutation(csrf);
  async function save(version: number, change: Change) {
    const saved = await mutation.run(
      endpoint(repo, `pulls/${pull}/review-requests`),
      "PUT",
      { version, change },
    );
    if (saved) {
      setOpen(false);
      setSelected(undefined);
      resource.retry();
      onSaved();
    }
  }
  return (
    <Result state={resource} timingLabel="Review requests">
      {(page) => {
        const requests = page.items.filter(
          (request) => request.state.kind !== "cancelled",
        );
        const canManage = page.can_manage && !repo.archived;
        const disabled = mutation.pending || mutation.conflict;
        const needle = query.trim().toLowerCase();
        const choices = page.candidates.filter((candidate) =>
          `${candidate.name} ${candidate.profile?.handle ?? ""}`
            .toLowerCase()
            .includes(needle),
        );
        return (
          <section className="review-requests" aria-label="Review requests">
            <header>
              <strong>Reviewers</strong>
              {canManage && (
                <Popover
                  open={open}
                  onOpenChange={(value) => {
                    setOpen(value);
                    if (value) setQuery("");
                  }}
                >
                  <PopoverTrigger asChild>
                    <Button
                      variant="ghost"
                      size="icon-sm"
                      aria-label="Request reviewers"
                      disabled={disabled}
                    >
                      <SettingsIcon />
                    </Button>
                  </PopoverTrigger>
                  {open && (
                    <PopoverContent
                      align="end"
                      aria-label="Request reviewers"
                      onOpenAutoFocus={(event) => {
                        event.preventDefault();
                        search.current?.focus();
                      }}
                      className="w-80 max-w-[calc(100vw-2rem)]"
                    >
                      <PopoverHeader className="flex-row items-center justify-between">
                        <PopoverTitle>Request reviewers</PopoverTitle>
                        <Button
                          variant="ghost"
                          size="icon-xs"
                          aria-label="Close reviewer picker"
                          onClick={() => setOpen(false)}
                        >
                          <XIcon />
                        </Button>
                      </PopoverHeader>
                      <Field>
                        <FieldLabel className="sr-only" htmlFor={searchId}>
                          Find a reviewer
                        </FieldLabel>
                        <Input
                          ref={search}
                          id={searchId}
                          placeholder="Find people…"
                          value={query}
                          onChange={(event) => setQuery(event.target.value)}
                        />
                      </Field>
                      <div className="flex max-h-64 flex-col overflow-y-auto">
                        {choices.map((candidate) => (
                          <Button
                            key={candidate.id}
                            variant={
                              selected === candidate.id ? "secondary" : "ghost"
                            }
                            className="h-auto min-h-10 justify-start"
                            aria-label={`Request review from ${candidate.profile?.handle ?? candidate.name}`}
                            disabled={disabled}
                            onClick={() => {
                              setSelected(candidate.id);
                              void save(page.version, {
                                kind: "request",
                                actor_id: candidate.id,
                              });
                            }}
                          >
                            <span className="flex min-w-0 flex-col items-start">
                              <span className="max-w-full truncate">
                                {candidate.profile?.handle ?? candidate.name}
                              </span>
                              {candidate.profile && (
                                <span className="max-w-full truncate text-muted-foreground">
                                  {candidate.name}
                                </span>
                              )}
                            </span>
                          </Button>
                        ))}
                        {!choices.length && (
                          <p
                            className="p-2 text-muted-foreground"
                            role="status"
                          >
                            {page.candidates.length
                              ? "No people match your search."
                              : "No other repository members are available to review."}
                          </p>
                        )}
                      </div>
                      {mutation.error && (
                        <FieldError>{mutation.error}</FieldError>
                      )}
                      {mutation.conflict && (
                        <Button
                          variant="outline"
                          size="sm"
                          onClick={() => {
                            resource.retry();
                            mutation.reset();
                          }}
                        >
                          Reload review requests
                        </Button>
                      )}
                    </PopoverContent>
                  )}
                </Popover>
              )}
            </header>
            {requests.length ? (
              <ul>
                {requests.map((request) => {
                  const candidateId = request.candidate_id;
                  const name =
                    request.reviewer.profile?.handle ?? request.reviewer.name;
                  return (
                    <li key={request.id}>
                      {request.state.kind === "fulfilled" ? (
                        <CodeReviewIcon aria-label="Review submitted" />
                      ) : (
                        <ClockIcon aria-label="Review requested" />
                      )}
                      <div>
                        <strong>
                          <Author author={request.reviewer} />
                        </strong>
                        <p className="muted">
                          {request.state.kind === "fulfilled" ? (
                            "Review submitted"
                          ) : (
                            <>
                              Requested by{" "}
                              <Author author={request.requested_by} />
                            </>
                          )}
                        </p>
                        {!candidateId && (
                          <p className="muted">
                            Unavailable for new review requests.
                          </p>
                        )}
                      </div>
                      {canManage &&
                        (request.state.kind === "requested" ? (
                          <Button
                            size="icon-xs"
                            variant="ghost"
                            aria-label={`Cancel review request for ${name}`}
                            disabled={disabled}
                            onClick={() =>
                              save(page.version, {
                                kind: "cancel",
                                id: request.id,
                              })
                            }
                          >
                            <XIcon />
                          </Button>
                        ) : (
                          candidateId && (
                            <Button
                              size="icon-xs"
                              variant="ghost"
                              aria-label={`Re-request review from ${name}`}
                              disabled={disabled}
                              onClick={() =>
                                save(page.version, {
                                  kind: "request",
                                  actor_id: candidateId,
                                })
                              }
                            >
                              <RotateCwIcon />
                            </Button>
                          )
                        ))}
                    </li>
                  );
                })}
              </ul>
            ) : (
              <p className="muted">No reviews requested.</p>
            )}
            <div className="discussion-actions">
              {before && (
                <Button
                  variant="outline"
                  size="sm"
                  onClick={() => setBefore(undefined)}
                >
                  First reviewer page
                </Button>
              )}
              {page.next !== null && (
                <Button
                  variant="outline"
                  size="sm"
                  onClick={() => setBefore(page.next ?? undefined)}
                >
                  More reviewers
                </Button>
              )}
            </div>
            {!open && mutation.error && (
              <FieldError>{mutation.error}</FieldError>
            )}
            {!open && mutation.conflict && (
              <Button
                variant="outline"
                size="sm"
                onClick={() => {
                  resource.retry();
                  mutation.reset();
                }}
              >
                Reload review requests
              </Button>
            )}
          </section>
        );
      }}
    </Result>
  );
}
