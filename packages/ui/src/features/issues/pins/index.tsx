import { useState } from "react";
import {
  GripVerticalIcon,
  MoreHorizontalIcon,
  PinIcon,
  PinOffIcon,
  CircleSlashIcon,
} from "lucide-react";
import {
  IssueOpenedIcon,
  IssueClosedIcon,
  CommentIcon,
} from "@primer/octicons-react";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardHeader,
  CardTitle,
  CardAction,
  CardContent,
  CardFooter,
} from "@/components/ui/card";
import {
  DropdownMenu,
  DropdownMenuTrigger,
  DropdownMenuContent,
  DropdownMenuGroup,
  DropdownMenuItem,
} from "@/components/ui/dropdown-menu";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Result, RequestWidget } from "@/components/request/result";
import { Author } from "@/components/discussion/author";
import { timestamp } from "@/components/discussion/time";
import { Link } from "@/components/navigation/link";
import { repoHref } from "@/lib/repository-route";
import { cn } from "@/lib/utils";
import type { Repository } from "@/lib/api";
import { usePins } from "./use";
import { useReturnFocus } from "@/components/discussion/hooks";

type Controller = ReturnType<typeof usePins>;
const DRAG = "application/x-crab-pinned-issue";

function Feedback({ pins }: { pins: Controller }) {
  return (
    <>
      <span className="sr-only" aria-live="polite">
        {pins.notice}
      </span>
      {pins.mutation.error && (
        <Alert variant="destructive">
          <AlertDescription>{pins.mutation.error}</AlertDescription>
        </Alert>
      )}
      {pins.recovering ? (
        <div className="flex flex-col items-start gap-2 text-sm" role="status">
          <p>
            The pin change may have completed. Recover it before changing the
            pinned issues.
          </p>
          <Button variant="outline" size="sm" onClick={pins.retry}>
            Retry original pin change
          </Button>
        </div>
      ) : (
        pins.mutation.error &&
        !pins.locked && (
          <Button variant="outline" size="sm" onClick={pins.reload}>
            Reload pinned issues
          </Button>
        )
      )}
    </>
  );
}

export function PinnedIssues({
  repo,
  csrf,
}: {
  repo: Repository;
  csrf: string;
}) {
  const pins = usePins(repo, csrf);
  const { resource } = pins;
  const [menu, setMenu] = useState<number>();
  const focus = useReturnFocus<HTMLElement>(
    pins.locked || (resource.loading && Boolean(pins.view)),
  );
  return (
    <RequestWidget timing={resource.timing} label="Pinned issues">
      <Result
        state={{
          ...resource,
          data: pins.view,
          loading: resource.loading && !pins.view,
          timing: undefined,
        }}
      >
        {(data) => {
          if (!data.items.length && !pins.notice) return null;
          const numbers = data.items.map((item) => item.number);
          const editable = data.can_manage && !repo.archived;
          const disabled =
            pins.locked ||
            resource.loading ||
            pins.mutation.conflict ||
            Boolean(pins.mutation.error);
          function move(from: number, to: number) {
            if (from === to || disabled) return;
            const next = [...numbers];
            const [number] = next.splice(from, 1);
            next.splice(to, 0, number);
            pins.save(next);
          }
          return (
            <section
              ref={focus}
              tabIndex={-1}
              aria-busy={resource.loading || pins.locked}
              aria-label="Pinned issues"
              className="flex flex-col gap-3"
            >
              <ol className="m-0 grid list-none grid-cols-1 gap-3 p-0 lg:grid-cols-3">
                {data.items.map((item, index) => {
                  const href = repoHref(repo, {
                    view: "issues",
                    issue: String(item.number),
                  });
                  const notPlanned =
                    item.state === "closed" &&
                    item.state_reason === "not_planned";
                  const Icon =
                    item.state === "open"
                      ? IssueOpenedIcon
                      : notPlanned
                        ? CircleSlashIcon
                        : IssueClosedIcon;
                  return (
                    <li
                      key={item.number}
                      onDragOver={(event) => {
                        if (
                          editable &&
                          !disabled &&
                          event.dataTransfer.types.includes(DRAG)
                        )
                          event.preventDefault();
                      }}
                      onDrop={(event) => {
                        if (!editable || disabled) return;
                        const value = event.dataTransfer.getData(DRAG);
                        if (!value) return;
                        event.preventDefault();
                        let source: unknown;
                        try {
                          source = JSON.parse(value);
                        } catch {
                          return;
                        }
                        if (
                          !source ||
                          typeof source !== "object" ||
                          !("repository" in source) ||
                          !("version" in source) ||
                          !("number" in source) ||
                          source.repository !== repo.id ||
                          source.version !== data.version
                        )
                          return;
                        const from = numbers.findIndex(
                          (number) => number === source.number,
                        );
                        if (from >= 0) move(from, index);
                      }}
                    >
                      <Card size="sm" className="h-full min-h-36 gap-2">
                        <CardHeader>
                          <CardTitle className="flex min-w-0 items-start gap-2">
                            <span
                              role="img"
                              aria-label={
                                notPlanned
                                  ? "Closed as not planned"
                                  : item.state
                              }
                              className={cn(
                                "issue-status-icon mt-0.5 shrink-0",
                                item.state,
                                notPlanned && "not-planned",
                              )}
                            >
                              <Icon />
                            </span>
                            <Link
                              href={href}
                              aria-label={`View ${item.title}`}
                              className="min-w-0 break-words"
                            >
                              {item.title}
                            </Link>
                          </CardTitle>
                          {editable && (
                            <CardAction className="flex items-center">
                              <Button
                                variant="ghost"
                                size="icon-xs"
                                disabled={disabled}
                                draggable={!disabled}
                                aria-label={`Reorder pinned issue #${item.number}`}
                                aria-haspopup="menu"
                                onClick={() => setMenu(item.number)}
                                onDragStart={(event) => {
                                  event.dataTransfer.effectAllowed = "move";
                                  event.dataTransfer.setData(
                                    DRAG,
                                    JSON.stringify({
                                      repository: repo.id,
                                      version: data.version,
                                      number: item.number,
                                    }),
                                  );
                                }}
                              >
                                <GripVerticalIcon data-icon="inline-start" />
                              </Button>
                              <DropdownMenu
                                open={menu === item.number}
                                onOpenChange={(open) =>
                                  setMenu(open ? item.number : undefined)
                                }
                              >
                                <DropdownMenuTrigger asChild>
                                  <Button
                                    variant="ghost"
                                    size="icon-xs"
                                    disabled={disabled}
                                    aria-label={`Pinned issue #${item.number} actions`}
                                  >
                                    <MoreHorizontalIcon data-icon="inline-start" />
                                  </Button>
                                </DropdownMenuTrigger>
                                <DropdownMenuContent align="end">
                                  <DropdownMenuGroup>
                                    <DropdownMenuItem
                                      disabled={index === 0}
                                      onSelect={() => move(index, index - 1)}
                                    >
                                      Move earlier
                                    </DropdownMenuItem>
                                    <DropdownMenuItem
                                      disabled={index === numbers.length - 1}
                                      onSelect={() => move(index, index + 1)}
                                    >
                                      Move later
                                    </DropdownMenuItem>
                                    <DropdownMenuItem
                                      onSelect={() =>
                                        pins.save(
                                          numbers.filter(
                                            (number) => number !== item.number,
                                          ),
                                        )
                                      }
                                    >
                                      <PinOffIcon data-icon="inline-start" />{" "}
                                      Unpin issue
                                    </DropdownMenuItem>
                                  </DropdownMenuGroup>
                                </DropdownMenuContent>
                              </DropdownMenu>
                            </CardAction>
                          )}
                        </CardHeader>
                        <CardContent className="flex flex-wrap items-center gap-x-1 text-xs text-muted-foreground">
                          <span>#{item.number} ·</span>
                          <Author author={item.author} />
                          <span>opened {timestamp(item.created_at)}</span>
                        </CardContent>
                        <CardFooter className="mt-auto justify-end">
                          <Link
                            href={href}
                            className="inline-flex items-center gap-1 text-xs text-muted-foreground"
                            aria-label={`${item.comment_count} comments on issue #${item.number}`}
                          >
                            <CommentIcon size={14} />
                            {item.comment_count}
                          </Link>
                        </CardFooter>
                      </Card>
                    </li>
                  );
                })}
              </ol>
            </section>
          );
        }}
      </Result>
      <Feedback pins={pins} />
    </RequestWidget>
  );
}

export function PinIssue({
  repo,
  csrf,
  number,
  onChanged,
}: {
  repo: Repository;
  csrf: string;
  number: number;
  onChanged: () => void;
}) {
  const pins = usePins(repo, csrf, onChanged);
  const { resource } = pins;
  const focus = useReturnFocus<HTMLButtonElement>(
    pins.locked || (resource.loading && Boolean(pins.view)),
  );
  const pinned = pins.view?.items.some((item) => item.number === number);
  const full = !pinned && (pins.view?.items.length ?? 0) >= 3;
  return (
    <RequestWidget timing={resource.timing} label="Issue pin">
      <Result
        state={{
          ...resource,
          data: pins.view,
          loading: resource.loading && !pins.view,
          timing: undefined,
        }}
      >
        {(data) =>
          data.can_manage ? (
            <section className="flex flex-col items-start gap-2">
              <Button
                ref={focus}
                variant="ghost"
                size="sm"
                disabled={
                  pins.locked ||
                  pins.mutation.conflict ||
                  Boolean(pins.mutation.error) ||
                  resource.loading ||
                  full ||
                  repo.archived
                }
                onClick={() =>
                  pins.save(
                    pinned
                      ? data.items
                          .filter((item) => item.number !== number)
                          .map((item) => item.number)
                      : [...data.items.map((item) => item.number), number],
                  )
                }
              >
                {pinned ? (
                  <PinOffIcon data-icon="inline-start" />
                ) : (
                  <PinIcon data-icon="inline-start" />
                )}
                {pinned ? "Unpin issue" : "Pin issue"}
              </Button>
              {full && (
                <p className="text-xs text-muted-foreground">
                  Three issues are already pinned. Unpin one before adding
                  another.
                </p>
              )}
            </section>
          ) : null
        }
      </Result>
      <Feedback pins={pins} />
    </RequestWidget>
  );
}
