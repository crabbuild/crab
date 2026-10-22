import { useState } from "react";
import {
  ChevronDownIcon,
  CircleCheckIcon,
  CircleSlashIcon,
  RotateCcwIcon,
} from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuLabel,
  DropdownMenuRadioGroup,
  DropdownMenuRadioItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Failure } from "@/components/discussion/failure";
import { useMutation } from "@/components/discussion/mutations";
import { useReturnFocus } from "@/components/discussion/hooks";
import { endpoint, type Repository } from "@/lib/api";
import type { Issue, IssueStateReason } from "./model";

export function IssueStateControls({
  issue,
  repo,
  csrf,
  disabled,
  onSaved,
  onRefresh,
}: {
  issue: Issue;
  repo: Repository;
  csrf: string;
  disabled: boolean;
  onSaved: (issue: Issue) => void;
  onRefresh: () => void;
}) {
  const [reason, setReason] =
    useState<Exclude<IssueStateReason, "reopened">>("completed");
  const mutation = useMutation(csrf);
  const trigger = useReturnFocus<HTMLButtonElement>(mutation.pending);
  const closed = issue.state === "closed";
  const pending = disabled || mutation.pending;
  async function change(
    state: Issue["state"],
    state_reason?: IssueStateReason,
  ) {
    const saved = await mutation.run<Issue>(
      endpoint(repo, `issues/${issue.number}`),
      "PATCH",
      {
        version: issue.version,
        state,
        state_reason,
      },
    );
    if (saved) {
      if (
        saved.state_reason === "completed" ||
        saved.state_reason === "not_planned"
      )
        setReason(saved.state_reason);
      onSaved(saved);
    }
  }
  const selected = closed ? (issue.state_reason ?? "") : reason;
  return (
    <div className="flex min-w-0 flex-col items-start gap-2">
      <div
        className="inline-flex items-center"
        role="group"
        aria-label="Issue state actions"
      >
        <Button
          type="button"
          variant="outline"
          className={closed ? "" : "rounded-r-none"}
          ref={trigger}
          disabled={pending}
          onClick={() =>
            change(closed ? "open" : "closed", closed ? undefined : reason)
          }
        >
          {closed ? (
            <RotateCcwIcon data-icon="inline-start" />
          ) : reason === "not_planned" ? (
            <CircleSlashIcon data-icon="inline-start" />
          ) : (
            <CircleCheckIcon data-icon="inline-start" />
          )}
          {closed
            ? "Reopen issue"
            : reason === "not_planned"
              ? "Close as not planned"
              : "Close issue"}
        </Button>
        <DropdownMenu>
          <DropdownMenuTrigger asChild>
            <Button
              type="button"
              variant="outline"
              size="icon"
              className={closed ? "ml-1" : "-ml-px rounded-l-none"}
              disabled={pending}
              aria-label={
                closed ? "Change close reason" : "Choose close reason"
              }
            >
              <ChevronDownIcon />
            </Button>
          </DropdownMenuTrigger>
          <DropdownMenuContent className="w-72" align="start">
            <DropdownMenuLabel>
              {closed ? "Change close reason" : "Close issue as"}
            </DropdownMenuLabel>
            <DropdownMenuRadioGroup
              value={selected}
              onValueChange={(value) => {
                const next = value as Exclude<IssueStateReason, "reopened">;
                if (closed) void change("closed", next);
                else setReason(next);
              }}
            >
              <DropdownMenuRadioItem value="completed">
                <CircleCheckIcon />
                <span>
                  Completed
                  <span className="block text-xs text-muted-foreground">
                    Done, fixed, or resolved.
                  </span>
                </span>
              </DropdownMenuRadioItem>
              <DropdownMenuRadioItem value="not_planned">
                <CircleSlashIcon />
                <span>
                  Not planned
                  <span className="block text-xs text-muted-foreground">
                    Won’t fix or won’t be implemented.
                  </span>
                </span>
              </DropdownMenuRadioItem>
            </DropdownMenuRadioGroup>
          </DropdownMenuContent>
        </DropdownMenu>
      </div>
      <Failure message={mutation.error} />
      {mutation.conflict && (
        <Button type="button" variant="outline" size="sm" onClick={onRefresh}>
          Refresh issue before retrying
        </Button>
      )}
    </div>
  );
}
