import { useId, useRef, useState } from "react";
import {
  ClockIcon,
  CodeReviewIcon,
  CheckCircleFillIcon,
  XCircleFillIcon,
} from "@primer/octicons-react";
import { RotateCwIcon, SettingsIcon, XIcon } from "lucide-react";
import { Author, type DiscussionAuthor } from "@/components/discussion/author";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { cn } from "@/lib/utils";
import { Link } from "@/components/navigation/link";
import { repoHref } from "@/lib/repository-route";
import type { ReviewState } from "@/components/discussion/conversation/model";
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

interface ReviewSummary {
  number: number;
  state: ReviewState;
  commit_oid: string;
  current: boolean | null;
}
interface Reviewer {
  id: string;
  reviewer: DiscussionAuthor;
  candidate_id: string | null;
  request: {
    id: string;
    requested_by: DiscussionAuthor;
    state:
      | { kind: "requested" | "cancelled" }
      | { kind: "fulfilled"; review: number };
  } | null;
  review: ReviewSummary | null;
  decision: ReviewSummary | null;
}
interface Page {
  items: Reviewer[];
  next: string | null;
  version: number;
  can_manage: boolean;
  candidates: RepositoryAssignee[];
}
type Change =
  | { kind: "request"; actor_id: string }
  | { kind: "cancel"; id: string };

export function Reviewers({
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
  const [after, setAfter] = useState<string>();
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [selected, setSelected] = useState<string>();
  const searchId = useId();
  const search = useRef<HTMLInputElement>(null);
  const resource = useRequest<Page>(
    endpoint(repo, `pulls/${pull}/reviewers`, { after }),
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
    <Result state={resource} timingLabel="Reviewers">
      {(page) => {
        const reviewers = page.items;
        const canManage = page.can_manage && !repo.archived;
        const disabled = mutation.pending || mutation.conflict;
        const needle = query.trim().toLowerCase();
        const choices = page.candidates.filter((candidate) =>
          `${candidate.name} ${candidate.profile?.handle ?? ""}`
            .toLowerCase()
            .includes(needle),
        );
        return (
          <section className="reviewers" aria-label="Reviewers">
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
            {reviewers.length ? (
              <ul className="m-0 flex list-none flex-col gap-3 p-0">
                {reviewers.map((reviewer) => {
                  const candidateId = reviewer.candidate_id;
                  const request = reviewer.request;
                  const summary = reviewer.decision ?? reviewer.review;
                  const requested = request?.state.kind === "requested";
                  const name =
                    reviewer.reviewer.profile?.handle ?? reviewer.reviewer.name;
                  const Icon =
                    summary?.current && summary.state === "approved"
                      ? CheckCircleFillIcon
                      : summary?.current &&
                          summary.state === "changes_requested"
                        ? XCircleFillIcon
                        : summary
                          ? CodeReviewIcon
                          : ClockIcon;
                  return (
                    <li key={reviewer.id} className="flex items-start gap-2">
                      <Icon
                        aria-hidden="true"
                        className={cn(
                          "mt-0.5 shrink-0",
                          summary?.current && summary.state === "approved"
                            ? "text-success"
                            : summary?.current &&
                                summary.state === "changes_requested"
                              ? "text-destructive"
                              : "text-muted-foreground",
                        )}
                      />
                      <div className="flex min-w-0 flex-1 flex-col gap-1 wrap-anywhere">
                        <strong>
                          <Author author={reviewer.reviewer} />
                        </strong>
                        {summary && (
                          <div className="flex flex-wrap gap-1">
                            <Badge
                              asChild
                              variant={
                                summary.current && summary.state === "approved"
                                  ? "success"
                                  : summary.current &&
                                      summary.state === "changes_requested"
                                    ? "destructive"
                                    : "secondary"
                              }
                            >
                              <Link
                                href={`${repoHref(repo, { view: "pulls", pull: String(pull) })}#pullrequestreview-${summary.number}`}
                                aria-label={`Open review #${summary.number} by ${name}`}
                              >
                                {summary.state === "approved"
                                  ? "Approved"
                                  : summary.state === "changes_requested"
                                    ? "Changes requested"
                                    : "Commented"}
                              </Link>
                            </Badge>
                            {summary.current === false && (
                              <Badge variant="outline">Outdated review</Badge>
                            )}
                            {summary.current === null && (
                              <Badge variant="outline">Head unavailable</Badge>
                            )}
                          </div>
                        )}
                        {requested && (
                          <p className="m-0 text-muted-foreground">
                            Review requested by{" "}
                            <Author author={request.requested_by} />
                          </p>
                        )}
                        {reviewer.decision &&
                          reviewer.review?.number !==
                            reviewer.decision.number && (
                            <p className="m-0 text-muted-foreground">
                              Commented after this decision.
                            </p>
                          )}
                        {!candidateId && (
                          <p className="m-0 text-muted-foreground">
                            Unavailable for new review requests.
                          </p>
                        )}
                      </div>
                      {canManage &&
                        (requested ? (
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
              <p className="text-muted-foreground">No reviews yet.</p>
            )}
            <div className="discussion-actions">
              {after && (
                <Button
                  variant="outline"
                  size="sm"
                  onClick={() => setAfter(undefined)}
                >
                  First reviewer page
                </Button>
              )}
              {page.next !== null && (
                <Button
                  variant="outline"
                  size="sm"
                  onClick={() => setAfter(page.next ?? undefined)}
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
