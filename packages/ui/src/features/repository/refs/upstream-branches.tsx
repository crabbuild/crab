import { GitBranchIcon, SearchIcon } from "@primer/octicons-react";
import { useRef, useState } from "react";

import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";
import {
  InputGroup,
  InputGroupAddon,
  InputGroupInput,
} from "@/components/ui/input-group";
import {
  endpoint,
  updateForkBranch,
  useRequest,
  type Ref,
  type Refs,
  type Repository,
} from "@/lib/api";
import { short } from "@/lib/format";

const branchPrefix = "refs/heads/";
const pageSize = 50;

export function UpstreamBranches({
  repo,
  upstream,
  refs,
  csrf,
  onRefresh,
}: {
  repo: Repository;
  upstream: NonNullable<Repository["forked_from"]>;
  refs: Refs;
  csrf: string;
  onRefresh: () => void;
}) {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [copying, setCopying] = useState<string>();
  const [error, setError] = useState<string>();
  const attempt = useRef<{ name: string; oid: string; id: string }>(undefined);
  const branches = useRequest<Refs>(
    open ? endpoint(upstream, "refs") : null,
    upstream.id,
  );
  const existing = new Set(refs.refs.map((ref) => ref.name));
  const normalizedQuery = query.trim().toLowerCase();

  async function copy(ref: Ref) {
    const prior = attempt.current;
    const requestId =
      prior?.name === ref.name && prior.oid === ref.oid
        ? prior.id
        : crypto.randomUUID();
    attempt.current = { name: ref.name, oid: ref.oid, id: requestId };
    setCopying(ref.name);
    setError(undefined);
    try {
      await updateForkBranch(
        repo,
        {
          branch: ref.name,
          fork_oid: null,
          upstream_oid: ref.oid,
          request_id: requestId,
        },
        csrf,
      );
      setOpen(false);
      onRefresh();
    } catch (failure) {
      setError(
        failure instanceof Error
          ? failure.message
          : "Could not copy the upstream branch.",
      );
    } finally {
      setCopying(undefined);
    }
  }

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        if (copying) return;
        if (next) {
          setQuery("");
          setError(undefined);
        }
        setOpen(next);
      }}
    >
      <DialogTrigger asChild>
        <Button variant="outline" size="sm">
          <GitBranchIcon data-icon="inline-start" /> Copy from upstream
        </Button>
      </DialogTrigger>
      <DialogContent showCloseButton={!copying}>
        <DialogHeader>
          <DialogTitle>Copy an upstream branch</DialogTitle>
          <DialogDescription>
            Choose a branch to copy into this fork under the same name.
          </DialogDescription>
        </DialogHeader>
        <InputGroup>
          <InputGroupInput
            aria-label="Search upstream branches"
            placeholder="Find an upstream branch…"
            value={query}
            onChange={(event) => setQuery(event.target.value)}
            disabled={!!copying}
          />
          <InputGroupAddon>
            <SearchIcon aria-hidden="true" />
          </InputGroupAddon>
        </InputGroup>
        <Result state={branches} timingLabel="Upstream branches">
          {(source) => {
            const available = source.refs.filter(
              (ref) =>
                ref.name.startsWith(branchPrefix) && !existing.has(ref.name),
            );
            const matching = available.filter((ref) =>
              ref.name
                .slice(branchPrefix.length)
                .toLowerCase()
                .includes(normalizedQuery),
            );
            const exact = matching.find(
              (ref) => ref.name === `${branchPrefix}${query.trim()}`,
            );
            const visible = exact
              ? [
                  exact,
                  ...matching
                    .filter((ref) => ref !== exact)
                    .slice(0, pageSize - 1),
                ]
              : matching.slice(0, pageSize);
            if (!matching.length)
              return (
                <p role="status" className="text-muted-foreground">
                  {available.length
                    ? "No upstream branches match your search."
                    : "All upstream branches are already in this fork."}
                </p>
              );
            return (
              <>
                <ul
                  aria-label="Branches available to copy"
                  className="grid max-h-64 gap-1 overflow-y-auto"
                >
                  {visible.map((ref) => {
                    const name = ref.name.slice(branchPrefix.length);
                    return (
                      <li key={ref.name} className="min-w-0">
                        <Button
                          type="button"
                          variant="ghost"
                          className="h-auto w-full justify-between gap-3 py-2 text-left whitespace-normal"
                          aria-label={`Copy ${name}`}
                          onClick={() => void copy(ref)}
                          disabled={!!copying}
                        >
                          <span className="min-w-0 wrap-anywhere">{name}</span>
                          <span className="shrink-0 font-mono text-xs text-muted-foreground">
                            {copying === ref.name ? "Copying…" : short(ref.oid)}
                          </span>
                        </Button>
                      </li>
                    );
                  })}
                </ul>
                {matching.length > pageSize && (
                  <p className="text-xs text-muted-foreground">
                    Search to find more branches.
                  </p>
                )}
              </>
            );
          }}
        </Result>
        {error && (
          <div className="grid justify-items-start gap-2">
            <p role="alert">{error}</p>
            <Button
              type="button"
              variant="outline"
              size="sm"
              onClick={() => {
                setOpen(false);
                onRefresh();
              }}
            >
              Refresh branches
            </Button>
          </div>
        )}
      </DialogContent>
    </Dialog>
  );
}
