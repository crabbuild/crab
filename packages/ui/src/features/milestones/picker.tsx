import { useEffect, useId, useRef, useState } from "react";
import { PencilIcon, XIcon } from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  Field,
  FieldError,
  FieldLabel,
  FieldLegend,
  FieldSet,
} from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import {
  Popover,
  PopoverContent,
  PopoverHeader,
  PopoverTitle,
  PopoverTrigger,
} from "@/components/ui/popover";
import { RadioGroup, RadioGroupItem } from "@/components/ui/radio-group";
import { Separator } from "@/components/ui/separator";
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group";
import { endpoint, useRequest, type Repository } from "@/lib/api";
import { useMutation } from "@/components/discussion/mutations";
import { SelectionReview } from "@/components/discussion/selection-review";
import { repoHref } from "@/lib/repository-route";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import type { Milestone, MilestoneAssignment } from "./model";
import { MilestoneDate, MilestoneProgress } from "./progress";

type Choice = Pick<Milestone, "number" | "title" | "state">;
interface Choices {
  items: Choice[];
  truncated: boolean;
}

export function MilestonePicker({
  repo,
  path,
  version,
  csrf,
  onSaved,
}: {
  repo: Repository;
  path: string;
  version: number;
  csrf: string;
  onSaved: () => void;
}) {
  const selection = useRequest<MilestoneAssignment>(
    endpoint(repo, `${path}/milestone`),
  );
  const [current, setCurrent] = useState<MilestoneAssignment>();
  useEffect(() => {
    if (selection.data) setCurrent(selection.data);
  }, [selection.data]);
  const observed = useRef(version);
  useEffect(() => {
    if (observed.current !== version) {
      observed.current = version;
      selection.retry();
    }
  }, [version, selection.retry]);
  const saved = current ?? selection.data;
  const [open, setOpen] = useState(false);
  const [chosen, setChosen] = useState<Choice | null>(null);
  const [editingVersion, setEditingVersion] = useState(0);
  const [query, setQuery] = useState("");
  const [requested, setRequested] = useState("");
  const [state, setState] = useState("open");
  useEffect(() => {
    const timer = setTimeout(() => setRequested(query), 150);
    return () => clearTimeout(timer);
  }, [query]);
  const catalog = useRequest<Choices>(
    open
      ? endpoint(repo, "discussion-search/milestones", {
          search: requested,
          state,
        })
      : null,
  );
  const search = useRef<HTMLInputElement>(null);
  const id = useId();
  const mutation = useMutation(csrf, repo);
  function changeOpen(next: boolean) {
    if (mutation.pending) return;
    if (next && saved) {
      setChosen(saved.milestone);
      setEditingVersion(saved.version);
      setQuery("");
      setRequested("");
      setState("open");
      mutation.reset();
      selection.retry();
    }
    setOpen(next);
  }
  return (
    <section className="milestone-metadata">
      <header>
        <strong>Milestone</strong>
        {saved?.can_manage && !repo.archived && (
          <Popover open={open} onOpenChange={changeOpen}>
            <PopoverTrigger asChild>
              <Button
                variant="ghost"
                size="icon-sm"
                aria-label="Edit milestone"
                disabled={mutation.pending}
              >
                <PencilIcon />
              </Button>
            </PopoverTrigger>
            {/* A fresh focus scope prevents closing animations from stealing focus on reopen. */}
            {open && (
              <PopoverContent
                align="end"
                className="block max-h-[var(--radix-popover-content-available-height)] w-80 max-w-[calc(100vw-2rem)] overflow-y-auto p-0"
                aria-label="Assign a milestone"
                inert={!open}
                onOpenAutoFocus={(event) => {
                  event.preventDefault();
                  search.current?.focus();
                }}
              >
                <PopoverHeader className="flex-row items-center justify-between p-3">
                  <PopoverTitle>Assign a milestone</PopoverTitle>
                  <Button
                    size="icon-sm"
                    variant="ghost"
                    aria-label="Close milestone picker"
                    disabled={mutation.pending}
                    onClick={() => changeOpen(false)}
                  >
                    <XIcon />
                  </Button>
                </PopoverHeader>
                <Separator />
                <Field className="p-3">
                  <FieldLabel htmlFor={id} className="sr-only">
                    Filter milestones
                  </FieldLabel>
                  <Input
                    id={id}
                    ref={search}
                    placeholder="Filter milestones"
                    maxLength={256}
                    value={query}
                    onChange={(event) => setQuery(event.target.value)}
                  />
                </Field>
                <ToggleGroup
                  type="single"
                  value={state}
                  size="sm"
                  className="mx-3 mb-3"
                  aria-label="Milestone state"
                  onValueChange={(value) => {
                    if (value) setState(value);
                  }}
                >
                  <ToggleGroupItem value="open" aria-label="Open milestones">
                    Open
                  </ToggleGroupItem>
                  <ToggleGroupItem
                    value="closed"
                    aria-label="Closed milestones"
                  >
                    Closed
                  </ToggleGroupItem>
                </ToggleGroup>
                <Result
                  state={{
                    ...catalog,
                    loading: catalog.loading || query !== requested,
                  }}
                  timingLabel="Milestone choices"
                >
                  {(page) => {
                    const choices =
                      chosen &&
                      !page.items.some(
                        (value) => value.number === chosen.number,
                      )
                        ? [chosen, ...page.items]
                        : page.items;
                    return (
                      <>
                        <div
                          role="region"
                          aria-label="Milestone choices"
                          tabIndex={0}
                          className="max-h-64 overflow-y-auto outline-none focus-visible:ring-2 focus-visible:ring-ring"
                        >
                          <FieldSet
                            disabled={
                              mutation.pending || Boolean(mutation.error)
                            }
                            className="gap-0 px-3 pb-3"
                          >
                            <FieldLegend className="sr-only">
                              Repository milestones
                            </FieldLegend>
                            <RadioGroup
                              value={chosen ? String(chosen.number) : "none"}
                              aria-label="Milestone assignment"
                              className="gap-0"
                              onValueChange={(value) =>
                                setChosen(
                                  choices.find(
                                    (item) => String(item.number) === value,
                                  ) ?? null,
                                )
                              }
                            >
                              <FieldLabel className="w-full cursor-pointer rounded-md p-2 hover:bg-accent">
                                <RadioGroupItem value="none" />
                                No milestone
                              </FieldLabel>
                              {choices.map((value) => (
                                <FieldLabel
                                  key={value.number}
                                  className="w-full cursor-pointer rounded-md p-2 hover:bg-accent"
                                >
                                  <RadioGroupItem
                                    value={String(value.number)}
                                    aria-label={value.title}
                                  />
                                  <span className="min-w-0 break-words">
                                    {value.title}
                                    <span className="block text-muted-foreground">
                                      {value.state === "closed"
                                        ? "Closed"
                                        : "Open"}
                                      {chosen?.number === value.number
                                        ? " · Selected"
                                        : ""}
                                    </span>
                                  </span>
                                </FieldLabel>
                              ))}
                            </RadioGroup>
                            {page.items.length === 0 && (
                              <p
                                className="p-2 text-muted-foreground"
                                role="status"
                              >
                                No milestones match your search.
                              </p>
                            )}
                            {page.truncated && (
                              <p
                                className="p-2 text-muted-foreground"
                                role="status"
                              >
                                More milestones match. Refine your search.
                              </p>
                            )}
                          </FieldSet>
                        </div>
                      </>
                    );
                  }}
                </Result>
                {mutation.error && (
                  <>
                    <FieldError className="px-3">
                      {mutation.status === undefined
                        ? "The response was lost. Review the saved selection before trying again."
                        : mutation.error}
                    </FieldError>
                    <SelectionReview<MilestoneAssignment>
                      url={endpoint(repo, `${path}/milestone`)}
                      label="Review current milestone"
                      canManage={(latest) =>
                        latest.can_manage && !repo.archived
                      }
                      onResolve={(latest, choice) => {
                        setEditingVersion(latest.version);
                        if (choice === "saved") setChosen(latest.milestone);
                        setCurrent(latest);
                        selection.retry();
                        mutation.reset();
                        onSaved();
                      }}
                    >
                      {(latest) => (
                        <div>
                          <strong>Saved milestone</strong>
                          <p>{latest.milestone?.title ?? "No milestone"}</p>
                        </div>
                      )}
                    </SelectionReview>
                  </>
                )}
                <Separator />
                <div className="flex flex-wrap gap-2 p-3">
                  <Button
                    size="sm"
                    disabled={
                      mutation.pending ||
                      Boolean(mutation.error) ||
                      !saved.can_manage ||
                      repo.archived
                    }
                    onClick={async () => {
                      const value = await mutation.run<MilestoneAssignment>(
                        endpoint(repo, `${path}/milestone`),
                        "PUT",
                        {
                          version: editingVersion,
                          milestone: chosen?.number ?? null,
                        },
                      );
                      if (value) {
                        setCurrent(value);
                        setOpen(false);
                        selection.retry();
                        onSaved();
                      }
                    }}
                  >
                    {mutation.pending ? "Saving…" : "Apply milestone"}
                  </Button>
                  <Button
                    size="sm"
                    variant="outline"
                    disabled={mutation.pending}
                    onClick={() => changeOpen(false)}
                  >
                    Cancel
                  </Button>
                </div>
              </PopoverContent>
            )}
          </Popover>
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
