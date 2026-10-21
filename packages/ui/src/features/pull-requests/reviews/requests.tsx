import { Button } from "@/components/ui/button";
import { useState } from "react";
import { Field, FieldGroup, FieldLabel } from "@/components/ui/field";
import {
  NativeSelect,
  NativeSelectOption,
} from "@/components/ui/native-select";
import { CheckCircleFillIcon, ClockIcon } from "@primer/octicons-react";
import { endpoint, useRequest, type Repository } from "@/lib/api";
import { useMutation } from "@/components/discussion/mutations";
import { Result } from "@/components/request/result";

interface ReviewRequest {
  id: string;
  name: string;
  available: boolean;
  requested_by: string;
  state:
    | { kind: "requested" | "cancelled" }
    | { kind: "fulfilled"; review: number };
}
interface Page {
  items: ReviewRequest[];
  next: string | null;
  version: number;
  can_manage: boolean;
  eligible: Array<{ subject: string; name: string }>;
}

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
  const [selected, setSelected] = useState("");
  const resource = useRequest<Page>(
    endpoint(repo, `pulls/${pull}/review-requests`, { before }),
  );
  const mutation = useMutation(csrf);
  async function save(
    version: number,
    change:
      | { kind: "request"; subject: string }
      | { kind: "cancel"; id: string },
  ) {
    const saved = await mutation.run(
      endpoint(repo, `pulls/${pull}/review-requests`),
      "PUT",
      { version, change },
    );
    if (saved) {
      setSelected("");
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
        return (
          <section className="review-requests" aria-label="Review requests">
            <header>
              <strong>Reviewers</strong>
            </header>
            {requests.length ? (
              <ul>
                {requests.map((request) => (
                  <li key={request.id}>
                    {request.state.kind === "fulfilled" ? (
                      <CheckCircleFillIcon
                        aria-label="Review submitted"
                        className="review-fulfilled"
                      />
                    ) : (
                      <ClockIcon aria-label="Review requested" />
                    )}
                    <div>
                      <strong>{request.name}</strong>
                      <p className="muted">
                        {request.state.kind === "fulfilled"
                          ? "Review submitted"
                          : `Requested by ${request.requested_by}`}
                      </p>
                      {!request.available && (
                        <p className="muted">No longer a repository member</p>
                      )}
                    </div>
                    {canManage && request.state.kind === "requested" && (
                      <Button
                        size="sm"
                        variant="ghost"
                        aria-label={`Cancel review request for ${request.name}`}
                        disabled={mutation.pending || mutation.conflict}
                        onClick={() =>
                          save(page.version, { kind: "cancel", id: request.id })
                        }
                      >
                        Cancel
                      </Button>
                    )}
                  </li>
                ))}
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
            {canManage &&
              (page.eligible.length ? (
                <form
                  onSubmit={(event) => {
                    event.preventDefault();
                    if (selected && !mutation.conflict)
                      void save(page.version, {
                        kind: "request",
                        subject: selected,
                      });
                  }}
                >
                  <FieldGroup>
                    <Field data-disabled={mutation.pending}>
                      <FieldLabel htmlFor="requested-reviewer">
                        Request a reviewer
                      </FieldLabel>
                      <NativeSelect
                        id="requested-reviewer"
                        required
                        disabled={mutation.pending}
                        value={selected}
                        onChange={(event) => setSelected(event.target.value)}
                      >
                        <NativeSelectOption value="">
                          Choose a repository member
                        </NativeSelectOption>
                        {page.eligible.map((member) => (
                          <NativeSelectOption
                            key={member.subject}
                            value={member.subject}
                          >
                            {member.name}
                          </NativeSelectOption>
                        ))}
                      </NativeSelect>
                    </Field>
                  </FieldGroup>

                  <Button
                    variant="outline"
                    type="submit"
                    size="sm"
                    disabled={
                      !selected || mutation.pending || mutation.conflict
                    }
                  >
                    {mutation.pending ? "Saving…" : "Request review"}
                  </Button>
                </form>
              ) : (
                <p className="muted">
                  No other repository members are available to review.
                </p>
              ))}
            {mutation.error && (
              <p className="error" role="alert">
                {mutation.error}
              </p>
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
          </section>
        );
      }}
    </Result>
  );
}
