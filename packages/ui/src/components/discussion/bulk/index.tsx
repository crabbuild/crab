import {
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { ChevronDownIcon } from "lucide-react";
import { endpoint, useRequest, type Repository } from "@/lib/api";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuGroup,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { useMutation } from "../mutations";
import { useReturnFocus } from "../hooks";
import { Spinner } from "@/components/ui/spinner";
import { RequestWidget } from "@/components/request/result";
import { Alert, AlertDescription } from "@/components/ui/alert";
import type { DiscussionItem } from "../row";
import { MetadataPicker, MilestonePicker } from "./pickers";

type Subject = { kind: "issue" | "pull"; number: number };
type Versioned = { subject: Subject; version: number };
type Selected = { item: Versioned; milestone_version: number };
export type BulkAction =
  | {
      kind: "state";
      state: "open" | "closed";
      reason?: "completed" | "not_planned";
    }
  | { kind: "labels"; add: number[]; remove: number[] }
  | { kind: "assignees"; add: string[]; remove: string[] }
  | { kind: "milestone"; number: number | null };
type Intent = {
  repository_id: string;
  request_id: string;
  items: Selected[];
  action: BulkAction;
};
const key = (item: DiscussionItem) => `${item.kind}:${item.number}`;

export function useBulkEdit(
  repo: Repository,
  csrf: string,
  items: DiscussionItem[],
  repositoryId: string | undefined,
  scope: string,
  refresh: () => void,
) {
  const [selected, setSelected] = useState<DiscussionItem[]>([]);
  const [selectedRepository, setSelectedRepository] = useState<string>();
  const [intent, setIntent] = useState<Intent>();
  const [message, setMessage] = useState("");
  const anchor = useRef<string | undefined>(undefined);
  const selectionScope = useRef(scope);
  const busy = useRef(false);
  const mutation = useMutation(csrf);
  const refreshLatest = useRef(refresh);
  useLayoutEffect(() => {
    refreshLatest.current = refresh;
  });
  const encoded = selected
    .map((item) => `${key(item)}:${item.version}`)
    .join(",");
  const identityChanged = Boolean(
    selected.length &&
    selectedRepository &&
    repositoryId &&
    selectedRepository !== repositoryId,
  );
  const prepared = useRequest<{ repository_id: string; items: Selected[] }>(
    selected.length && selectedRepository && !intent && !identityChanged
      ? endpoint(repo, "discussions/selection", {
          repository_id: selectedRepository,
          items: encoded,
        })
      : null,
  );
  const locked = mutation.pending || Boolean(intent);
  useEffect(() => {
    // Navigation clears an idle selection. An uncertain write retains its
    // original targets until recovery, even if a filter changed meanwhile.
    if (selectionScope.current !== scope && !intent) {
      setSelected([]);
      setSelectedRepository(undefined);
      selectionScope.current = scope;
      anchor.current = undefined;
    }
  }, [scope, intent]);
  useEffect(() => {
    if (intent && !mutation.pending && mutation.rejected) setIntent(undefined);
  }, [intent, mutation.pending, mutation.rejected]);
  async function run(proposed: Intent) {
    if (busy.current) return false;
    busy.current = true;
    const attempt = intent ?? proposed;
    setIntent(attempt);
    try {
      const saved = await mutation.run<{ count: number }>(
        endpoint(repo, "discussions/bulk"),
        "POST",
        attempt,
      );
      if (saved) {
        setIntent(undefined);
        setSelected([]);
        setSelectedRepository(undefined);
        setMessage(
          `Updated ${saved.count} selected ${saved.count === 1 ? "item" : "items"}.`,
        );
        refreshLatest.current();
        return true;
      }
      return false;
    } finally {
      busy.current = false;
    }
  }
  return {
    selected,
    locked,
    pending: mutation.pending,
    error: identityChanged
      ? "Repository identity changed. Reload the selection before editing."
      : mutation.error,
    conflict: mutation.conflict || identityChanged,
    identityChanged,
    prepared,
    message,
    recovering: Boolean(intent && !mutation.pending && !mutation.rejected),
    toggle(item: DiscussionItem, range: boolean) {
      if (locked || identityChanged || !repositoryId) return;
      if (!selected.length) setSelectedRepository(repositoryId);
      const checked = !selected.some((value) => key(value) === key(item));
      const a = items.findIndex((value) => key(value) === anchor.current);
      const b = items.findIndex((value) => key(value) === key(item));
      const changed =
        range && a >= 0
          ? items.slice(Math.min(a, b), Math.max(a, b) + 1)
          : [item];
      const keys = new Set(changed.map(key));
      setSelected((previous) => [
        ...previous.filter((value) => !keys.has(key(value))),
        ...(checked ? changed : []),
      ]);
      anchor.current = key(item);
      setMessage("");
      mutation.reset();
    },
    selectAll() {
      if (!locked && !identityChanged && repositoryId) {
        setSelectedRepository(repositoryId);
        setSelected(selected.length === items.length ? [] : items);
        setMessage("");
        mutation.reset();
      }
    },
    checked(item: DiscussionItem) {
      return selected.some((value) => key(value) === key(item));
    },
    clear() {
      if (!locked) {
        setSelected([]);
        setSelectedRepository(undefined);
        mutation.reset();
      }
    },
    reload() {
      if (!locked) {
        setSelected([]);
        setSelectedRepository(undefined);
        mutation.reset();
        refresh();
      }
    },
    submit(action: BulkAction) {
      if (
        !prepared.data ||
        prepared.loading ||
        locked ||
        mutation.conflict ||
        !selectedRepository ||
        identityChanged
      )
        return Promise.resolve(false);
      return run({
        repository_id: selectedRepository,
        request_id: crypto.randomUUID(),
        items: prepared.data.items,
        action,
      });
    },
    retry() {
      return intent ? run(intent) : Promise.resolve(false);
    },
  };
}
type Controller = ReturnType<typeof useBulkEdit>;

export function BulkCheckbox({
  bulk,
  count,
  disabled,
  loading,
}: {
  bulk: Controller;
  count: number;
  disabled: boolean;
  loading: boolean;
}) {
  const focus = useReturnFocus<HTMLButtonElement>(bulk.pending || loading);
  return (
    <Checkbox
      ref={focus}
      className="mx-1"
      aria-label="Select all items on this page"
      disabled={disabled || bulk.locked || bulk.identityChanged || !count}
      checked={
        bulk.selected.length
          ? bulk.selected.length === count
            ? true
            : "indeterminate"
          : false
      }
      onCheckedChange={bulk.selectAll}
    />
  );
}

export function BulkActions({
  repo,
  bulk,
  selection,
}: {
  repo: Repository;
  bulk: Controller;
  selection: ReactNode;
}) {
  const recovery = useReturnFocus<HTMLButtonElement>(bulk.pending);
  if (!bulk.selected.length && !bulk.recovering)
    return (
      <div className="sr-only" aria-live="polite">
        {bulk.message}
      </div>
    );
  const disabled =
    bulk.locked ||
    bulk.prepared.loading ||
    !bulk.prepared.data ||
    bulk.conflict ||
    repo.archived;
  const hasPulls = bulk.selected.some((item) => item.kind === "pull");
  const hasMerged = bulk.selected.some(
    (item) => item.kind === "pull" && item.state === "merged",
  );
  return (
    <RequestWidget timing={bulk.prepared.timing} label="Bulk selection">
      <div
        className="flex min-w-0 flex-col gap-2 border-b bg-muted px-4 py-3"
        aria-label="Bulk discussion actions"
        role="region"
      >
        <div className="flex flex-wrap items-center gap-2">
          {selection}
          <strong className="text-sm" aria-live="polite">
            {bulk.selected.length} selected
          </strong>
          <div className="ml-auto flex flex-wrap items-center gap-1">
            <DropdownMenu>
              <DropdownMenuTrigger asChild>
                <Button
                  variant="ghost"
                  size="sm"
                  disabled={disabled || hasMerged}
                >
                  Mark as <ChevronDownIcon data-icon="inline-end" />
                </Button>
              </DropdownMenuTrigger>
              <DropdownMenuContent className="w-56" align="end">
                <DropdownMenuGroup>
                  <DropdownMenuItem
                    onSelect={() =>
                      void bulk.submit({ kind: "state", state: "open" })
                    }
                  >
                    Open
                  </DropdownMenuItem>
                  <DropdownMenuItem
                    onSelect={() =>
                      void bulk.submit({ kind: "state", state: "closed" })
                    }
                  >
                    Closed{!hasPulls && " as completed"}
                  </DropdownMenuItem>
                  {!hasPulls && (
                    <DropdownMenuItem
                      onSelect={() =>
                        void bulk.submit({
                          kind: "state",
                          state: "closed",
                          reason: "not_planned",
                        })
                      }
                    >
                      Closed as not planned
                    </DropdownMenuItem>
                  )}
                </DropdownMenuGroup>
              </DropdownMenuContent>
            </DropdownMenu>
            <MetadataPicker
              repo={repo}
              kind="labels"
              selected={bulk.selected}
              disabled={disabled}
              onApply={bulk.submit}
            />
            <MetadataPicker
              repo={repo}
              kind="assignees"
              selected={bulk.selected}
              disabled={disabled}
              onApply={bulk.submit}
            />
            <MilestonePicker
              repo={repo}
              disabled={disabled}
              onApply={bulk.submit}
            />
            <Button
              variant="ghost"
              size="sm"
              disabled={bulk.locked}
              onClick={bulk.clear}
            >
              Clear selection
            </Button>
          </div>
        </div>
        {bulk.prepared.loading && !bulk.locked && (
          <p role="status" className="text-sm text-muted-foreground">
            Checking selected versions…
          </p>
        )}
        {bulk.pending && (
          <p
            role="status"
            className="flex items-center gap-2 text-sm text-muted-foreground"
          >
            <Spinner />
            Updating selected items…
          </p>
        )}
        {(bulk.error ?? bulk.prepared.error) && (
          <Alert variant="destructive">
            <AlertDescription>
              {bulk.error ?? bulk.prepared.error} Your selection is preserved.
            </AlertDescription>
          </Alert>
        )}
        {(bulk.conflict || bulk.prepared.error) && !bulk.locked && (
          <Button
            variant="outline"
            size="sm"
            className="self-start"
            onClick={bulk.reload}
          >
            Reload selected items
          </Button>
        )}
        {bulk.recovering && (
          <div
            role="status"
            className="flex flex-col items-start gap-2 text-sm"
          >
            <p>
              The bulk action may have completed. Recover the original action
              before changing the selection.
            </p>
            <Button
              ref={recovery}
              variant="outline"
              onClick={() => void bulk.retry()}
            >
              Retry original bulk action
            </Button>
          </div>
        )}
      </div>
    </RequestWidget>
  );
}
