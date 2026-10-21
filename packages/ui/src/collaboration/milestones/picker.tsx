import { useRef, useState } from "react";
import { Button } from "@primer/react";
import { PencilIcon } from "@primer/octicons-react";
import { endpoint, useRequest, type Repository } from "../../api";
import { useMutation } from "../../discussion-mutations";
import { repoHref } from "../../repository-route";
import { Link, Result } from "../../ui";
import type { MilestoneAssignment, MilestonePage } from "./model";
import { MilestoneDate, MilestoneProgress } from "./progress";

export function MilestonePicker({
  repo,
  path,
  csrf,
}: {
  repo: Repository;
  path: string;
  csrf: string;
}) {
  const selection = useRequest<MilestoneAssignment>(
    endpoint(repo, `${path}/milestone`),
  );
  const details = useRef<HTMLDetailsElement>(null);
  const [open, setOpen] = useState(false);
  const [chosen, setChosen] = useState("");
  const [chosenTitle, setChosenTitle] = useState("");
  const [before, setBefore] = useState<string>();
  const catalog = useRequest<MilestonePage>(
    open
      ? endpoint(repo, "milestones", { state: "open", before, limit: "50" })
      : null,
  );
  const mutation = useMutation(csrf);
  return (
    <section className="milestone-metadata">
      <header>
        <strong>Milestone</strong>
        {selection.data?.can_manage && !repo.archived && (
          <details
            className="milestone-picker"
            ref={details}
            onToggle={(event) => {
              setOpen(event.currentTarget.open);
              if (event.currentTarget.open) {
                setChosen(
                  selection.data?.milestone
                    ? String(selection.data.milestone.number)
                    : "",
                );
                setChosenTitle(selection.data?.milestone?.title ?? "");
                mutation.reset();
              }
            }}
          >
            <summary aria-label="Edit milestone">
              <PencilIcon />
            </summary>
            <div className="milestone-picker-menu">
              <label htmlFor="discussion-milestone">Assign a milestone</label>
              <Result state={catalog} timingLabel="Open milestones">
                {(page) => (
                  <>
                    <select
                      id="discussion-milestone"
                      value={chosen}
                      onChange={(event) => {
                        setChosen(event.target.value);
                        setChosenTitle(
                          page.items.find(
                            (value) =>
                              String(value.number) === event.target.value,
                          )?.title ?? "",
                        );
                      }}
                      disabled={mutation.pending}
                    >
                      <option value="">No milestone</option>
                      {chosen &&
                        !page.items.some(
                          (value) => String(value.number) === chosen,
                        ) && (
                          <option value={chosen}>
                            {chosenTitle || `Milestone #${chosen}`}
                          </option>
                        )}
                      {page.items.map((value) => (
                        <option key={value.number} value={value.number}>
                          {value.title}
                        </option>
                      ))}
                    </select>
                    <div className="milestone-picker-pages">
                      {before && (
                        <Button
                          size="small"
                          variant="invisible"
                          onClick={() => setBefore(undefined)}
                        >
                          First page
                        </Button>
                      )}
                      {page.next !== null && (
                        <Button
                          size="small"
                          variant="invisible"
                          onClick={() => setBefore(String(page.next))}
                        >
                          More milestones
                        </Button>
                      )}
                    </div>
                  </>
                )}
              </Result>
              {mutation.error && (
                <p className="error" role="alert">
                  {mutation.error}
                </p>
              )}
              {mutation.conflict && (
                <Button
                  size="small"
                  onClick={() => {
                    selection.retry();
                    details.current?.removeAttribute("open");
                  }}
                >
                  Reload current assignment
                </Button>
              )}
              <div className="discussion-actions">
                <Button
                  size="small"
                  variant="primary"
                  disabled={mutation.pending || !selection.data}
                  onClick={async () => {
                    if (!selection.data) return;
                    const value = await mutation.run<MilestoneAssignment>(
                      endpoint(repo, `${path}/milestone`),
                      "PUT",
                      {
                        version: selection.data.version,
                        milestone: chosen ? Number(chosen) : null,
                      },
                    );
                    if (value) {
                      details.current?.removeAttribute("open");
                      selection.retry();
                    }
                  }}
                >
                  {mutation.pending ? "Saving…" : "Apply milestone"}
                </Button>
                <Button
                  size="small"
                  onClick={() => details.current?.removeAttribute("open")}
                >
                  Cancel
                </Button>
              </div>
            </div>
          </details>
        )}
      </header>
      <Result state={selection} timingLabel="Discussion milestone">
        {({ milestone }) =>
          milestone ? (
            <>
              <Link
                href={repoHref(repo, {
                  view: "milestones",
                  milestone: String(milestone.number),
                })}
              >
                {milestone.title}
              </Link>
              <MilestoneDate milestone={milestone} />
              <MilestoneProgress milestone={milestone} />
            </>
          ) : (
            <p className="muted">No milestone</p>
          )
        }
      </Result>
    </section>
  );
}
