import { ChevronDownIcon } from "lucide-react";
import { endpoint, useRequest, type Repository } from "@/lib/api";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuRadioGroup,
  DropdownMenuRadioItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { RequestWidget } from "@/components/request/result";
import { useRef, useState } from "react";

type Choice = { query: string | null; selected: boolean };
type Menu = { clear: Choice; items: Array<Choice & { label: string }> };
export function ReviewFilter({
  repo,
  query,
  onSelect,
}: {
  repo: Repository;
  query: string;
  onSelect: (value: string) => void;
}) {
  const [open, setOpen] = useState(false);
  const content = useRef<HTMLDivElement>(null);
  const trigger = useRef<HTMLButtonElement>(null);
  const selected = useRef(false);
  const menu = useRequest<Menu>(
    open ? endpoint(repo, "discussion-search/reviews", { query }) : null,
  );
  function choose(choice: Choice) {
    if (choice.query === null) return;
    selected.current = true;
    onSelect(choice.query);
  }
  return (
    <RequestWidget timing={menu.timing} label="Review filters">
      <DropdownMenu open={open} onOpenChange={setOpen} modal={false}>
        <DropdownMenuTrigger asChild>
          <Button
            ref={trigger}
            variant="ghost"
            size="sm"
            aria-label="Filter by reviews"
          >
            Reviews <ChevronDownIcon size={14} />
          </Button>
        </DropdownMenuTrigger>
        <DropdownMenuContent
          ref={content}
          onCloseAutoFocus={(event) => {
            if (!selected.current) return;
            selected.current = false;
            event.preventDefault();
            trigger.current?.focus();
          }}
          align="end"
          collisionPadding={16}
          className="max-h-[calc(100svh-2rem)] w-80 max-w-[calc(100vw-2rem)] overflow-y-auto"
        >
          {menu.loading && (
            <p role="status" className="p-2 text-sm text-muted-foreground">
              Loading review filters…
            </p>
          )}
          {menu.error && (
            <div role="alert" className="p-2 text-sm text-destructive">
              <p>{menu.error}</p>
              <DropdownMenuItem
                onSelect={(event) => {
                  event.preventDefault();
                  content.current?.focus();
                  menu.retry();
                }}
              >
                Retry review filters
              </DropdownMenuItem>
            </div>
          )}
          {!menu.loading && !menu.error && menu.data && (
            <>
              <DropdownMenuRadioGroup
                aria-label="Review filter choices"
                value={
                  menu.data.items.find((choice) => choice.selected)?.label ?? ""
                }
                onValueChange={(value) => {
                  const choice = menu.data?.items.find(
                    (choice) => choice.label === value,
                  );
                  if (choice) choose(choice);
                }}
              >
                {menu.data.items.map((choice) => (
                  <DropdownMenuRadioItem
                    key={choice.label}
                    value={choice.label}
                    disabled={choice.query === null}
                  >
                    {choice.label}
                  </DropdownMenuRadioItem>
                ))}
                <DropdownMenuRadioItem value="team-review-unavailable" disabled>
                  <span>
                    Awaiting review from you or your team
                    <small className="block text-xs text-muted-foreground">
                      Team review requests are not yet supported.
                    </small>
                  </span>
                </DropdownMenuRadioItem>
              </DropdownMenuRadioGroup>
              {menu.data.items.some((choice) => choice.query === null) && (
                <p className="p-2 text-xs text-muted-foreground">
                  Shorten the search query to enable the remaining review
                  filters.
                </p>
              )}
              <DropdownMenuSeparator />
              <DropdownMenuItem
                disabled={menu.data.clear.query === null}
                onSelect={() => {
                  if (menu.data) choose(menu.data.clear);
                }}
              >
                Clear review filters
              </DropdownMenuItem>
            </>
          )}
        </DropdownMenuContent>
      </DropdownMenu>
    </RequestWidget>
  );
}
