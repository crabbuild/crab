import { useState } from "react";
import { Button, FormControl, Select } from "@primer/react";
import { CheckCircleFillIcon, ClockIcon } from "@primer/octicons-react";
import { endpoint, useRequest, type Repository } from "../../api";
import { useMutation } from "../../discussion-mutations";
import { Result } from "../../ui";

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
                        size="small"
                        variant="invisible"
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
                <Button size="small" onClick={() => setBefore(undefined)}>
                  First reviewer page
                </Button>
              )}
              {page.next !== null && (
                <Button
                  size="small"
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
                  <FormControl required disabled={mutation.pending}>
                    <FormControl.Label>Request a reviewer</FormControl.Label>
                    <Select
                      block
                      value={selected}
                      onChange={(event) => setSelected(event.target.value)}
                    >
                      <Select.Option value="">
                        Choose a repository member
                      </Select.Option>
                      {page.eligible.map((member) => (
                        <Select.Option
                          key={member.subject}
                          value={member.subject}
                        >
                          {member.name}
                        </Select.Option>
                      ))}
                    </Select>
                  </FormControl>
                  <Button
                    type="submit"
                    size="small"
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
                size="small"
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
