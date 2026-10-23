import { useEffect, useRef, useState } from "react";
import { PencilIcon, XIcon } from "lucide-react";
import { Button } from "@/components/ui/button";
import { FieldError } from "@/components/ui/field";
import {
  Popover,
  PopoverContent,
  PopoverHeader,
  PopoverTitle,
  PopoverTrigger,
} from "@/components/ui/popover";
import { Separator } from "@/components/ui/separator";
import { endpoint, useRequest, type Repository } from "@/lib/api";
import { useMutation } from "@/components/discussion/mutations";
import { SelectionReview } from "@/components/discussion/selection-review";
import { repoHref } from "@/lib/repository-route";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import type { MilestoneAssignment } from "./model";
import { MilestoneDate, MilestoneProgress } from "./progress";
import { MilestoneChoices, type MilestoneChoice as Choice } from "./choices";

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
  const search = useRef<HTMLInputElement>(null);
  const mutation = useMutation(csrf, repo);
  function changeOpen(next: boolean) {
    if (mutation.pending) return;
    if (next && saved) {
      setChosen(saved.milestone);
      setEditingVersion(saved.version);
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
                <MilestoneChoices
                  repo={repo}
                  chosen={chosen}
                  onChange={setChosen}
                  disabled={mutation.pending || Boolean(mutation.error)}
                  inputRef={search}
                />
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
