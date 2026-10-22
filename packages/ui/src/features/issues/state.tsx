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
import type { StateChange } from "@/components/discussion/conversation/compose";
import { useReturnFocus } from "@/components/discussion/hooks";
import type { Issue, IssueStateReason } from "./model";

export function IssueStateControls({
  issue,
  disabled,
  pending,
  refreshing,
  conflict,
  withComment,
  onChange,
  onRefresh,
}: {
  issue: Issue;
  disabled: boolean;
  pending: boolean;
  refreshing: boolean;
  conflict: boolean;
  withComment: boolean;
  onChange: (change: StateChange, mode?: "include" | "omit") => Promise<void>;
  onRefresh: () => void;
}) {
  const [reason, setReason] =
    useState<Exclude<IssueStateReason, "reopened">>("completed");
  const trigger = useReturnFocus<HTMLButtonElement>(pending || refreshing);
  const closed = issue.state === "closed";
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
          ref={conflict ? undefined : trigger}
          disabled={disabled || conflict}
          onClick={() =>
            onChange({
              version: issue.version,
              state: closed ? "open" : "closed",
              reason: closed ? undefined : reason,
            })
          }
        >
          {closed ? (
            <RotateCcwIcon data-icon="inline-start" />
          ) : reason === "not_planned" ? (
            <CircleSlashIcon data-icon="inline-start" />
          ) : (
            <CircleCheckIcon data-icon="inline-start" />
          )}
          {withComment
            ? closed
              ? "Reopen with comment"
              : "Close with comment"
            : closed
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
              disabled={disabled || conflict}
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
                setReason(next);
                if (closed)
                  void onChange(
                    { version: issue.version, state: "closed", reason: next },
                    "omit",
                  );
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
      {conflict && (
        <Button
          ref={trigger}
          type="button"
          variant="outline"
          size="sm"
          onClick={onRefresh}
        >
          Refresh issue before retrying
        </Button>
      )}
    </div>
  );
}
