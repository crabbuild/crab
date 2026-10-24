import { Badge } from "@/components/ui/badge";
import {
  InputGroup,
  InputGroupInput,
  InputGroupAddon,
} from "@/components/ui/input-group";
import { Button } from "@/components/ui/button";
import { useId, useRef, useState } from "react";
import {
  Popover,
  PopoverTrigger,
  PopoverContent,
  PopoverTitle,
} from "@/components/ui/popover";
import { Tabs, TabsList, TabsTrigger, TabsContent } from "@/components/ui/tabs";
import * as RovingFocus from "@radix-ui/react-roving-focus";
import { CheckIcon } from "lucide-react";
import {
  GitBranchIcon,
  GitCommitIcon,
  SearchIcon,
  TagIcon,
  TriangleDownIcon,
  XIcon,
} from "@primer/octicons-react";
import type { Refs } from "@/lib/api";
import { short } from "@/lib/format";

type RefType = "branches" | "tags";

export function RevisionPicker({
  refs,
  revision,
  onSelect,
  onCreateBranch,
}: {
  refs: Refs;
  revision: string;
  onSelect: (name: string) => void;
  onCreateBranch?: (name: string) => Promise<void>;
}) {
  const [open, setOpen] = useState(false);
  const [type, setType] = useState<RefType>("branches");
  const [filter, setFilter] = useState("");
  const [creating, setCreating] = useState(false);
  const [createError, setCreateError] = useState("");
  const input = useRef<HTMLInputElement>(null);
  const menu = useRef<HTMLDivElement>(null);
  const createAction = useRef<HTMLButtonElement>(null);
  const id = useId();
  const selected = refs.refs.find((ref) => ref.name === revision);
  const label = selected
    ? selected.name.replace(/^refs\/(heads|tags)\//, "")
    : short(revision);
  const Icon = !selected
    ? GitCommitIcon
    : selected.name.startsWith("refs/tags/")
      ? TagIcon
      : GitBranchIcon;
  const prefix = type === "branches" ? "refs/heads/" : "refs/tags/";
  const matches = refs.refs.filter(
    (ref) =>
      ref.name.startsWith(prefix) &&
      ref.name
        .slice(prefix.length)
        .toLowerCase()
        .includes(filter.toLowerCase()),
  );
  const candidate = filter.trim();
  const canCreate =
    type === "branches" &&
    Boolean(onCreateBranch) &&
    candidate.length > 0 &&
    candidate === filter &&
    !refs.refs.some((ref) => ref.name === `refs/heads/${candidate}`);
  async function createBranch() {
    if (!onCreateBranch || !canCreate || creating) return;
    setCreating(true);
    setCreateError("");
    try {
      await onCreateBranch(candidate);
      setOpen(false);
    } catch (error) {
      setCreateError(
        error instanceof Error
          ? error.message
          : "The branch could not be created",
      );
    } finally {
      setCreating(false);
    }
  }
  return (
    <Popover
      open={open}
      onOpenChange={(next) => {
        if (next) {
          setType(revision.startsWith("refs/tags/") ? "tags" : "branches");
          setFilter("");
          setCreateError("");
        }
        setOpen(next);
      }}
    >
      <PopoverTrigger asChild>
        <Button
          variant="outline"
          className="revision-picker-anchor justify-start"
          aria-label={`Switch branches or tags, current ${label}`}
          title={revision}
        >
          <Icon data-icon="inline-start" />
          <span className="min-w-0 flex-1 truncate text-left">{label}</span>
          <TriangleDownIcon data-icon="inline-end" />
        </Button>
      </PopoverTrigger>
      <PopoverContent
        align="start"
        className="revision-picker-panel w-80 gap-0 p-0"
        aria-labelledby={`${id}-title`}
        onOpenAutoFocus={(event) => {
          event.preventDefault();
          input.current?.focus();
        }}
      >
        <header className="revision-picker-heading">
          <PopoverTitle id={`${id}-title`}>Switch branches/tags</PopoverTitle>
          <Button
            aria-label="Close branch picker"
            variant="ghost"
            size="icon-sm"
            onClick={() => setOpen(false)}
          >
            <XIcon data-icon="inline-start" />
          </Button>
        </header>
        <div className="revision-picker-search">
          <InputGroup>
            <InputGroupInput
              ref={input}
              aria-label={`Filter ${type}`}
              placeholder={`Find a ${type === "branches" ? "branch" : "tag"}…`}
              value={filter}
              onChange={(event) => {
                setFilter(event.target.value);
                setCreateError("");
              }}
              onKeyDown={(event) => {
                if (event.key === "ArrowDown") {
                  event.preventDefault();
                  const first = menu.current?.querySelector<HTMLElement>(
                    '[role="menuitemradio"]',
                  );
                  (first ?? createAction.current)?.focus();
                } else if (
                  event.key === "Enter" &&
                  canCreate &&
                  !matches.length
                ) {
                  event.preventDefault();
                  void createBranch();
                }
              }}
            />
            <InputGroupAddon>
              <SearchIcon aria-hidden="true" />
            </InputGroupAddon>
          </InputGroup>
        </div>
        <Tabs
          value={type}
          onValueChange={(value) => {
            if (value !== "branches" && value !== "tags") return;
            setType(value);
            setCreateError("");
          }}
        >
          <TabsList variant="line" className="mx-2" aria-label="Ref type">
            <TabsTrigger value="branches">Branches</TabsTrigger>
            <TabsTrigger value="tags">Tags</TabsTrigger>
          </TabsList>
          <TabsContent value={type} className="revision-picker-results">
            {matches.length ? (
              <RovingFocus.Root
                ref={menu}
                role="menu"
                orientation="vertical"
                loop
                aria-label={type === "branches" ? "Branches" : "Tags"}
                className="flex flex-col gap-1 p-2"
              >
                {matches.map((ref) => (
                  <RovingFocus.Item
                    key={ref.name}
                    asChild
                    active={revision === ref.name}
                  >
                    <Button
                      variant={revision === ref.name ? "secondary" : "ghost"}
                      className="w-full justify-start"
                      role="menuitemradio"
                      aria-checked={revision === ref.name}
                      onClick={() => {
                        setOpen(false);
                        onSelect(ref.name);
                      }}
                    >
                      <span className="min-w-0 flex-1 truncate text-left">
                        {ref.name.slice(prefix.length)}
                      </span>
                      {refs.head?.name === ref.name && (
                        <Badge variant="outline">default</Badge>
                      )}
                      {revision === ref.name && (
                        <CheckIcon data-icon="inline-end" />
                      )}
                    </Button>
                  </RovingFocus.Item>
                ))}
              </RovingFocus.Root>
            ) : (
              <p className="revision-picker-empty" role="status">
                {filter ? `No ${type} match “${filter}”.` : `No ${type} yet.`}
              </p>
            )}
          </TabsContent>
        </Tabs>
        {canCreate && (
          <button
            ref={createAction}
            type="button"
            className="revision-picker-create"
            disabled={creating}
            onClick={() => void createBranch()}
          >
            <GitBranchIcon />
            <span>
              {creating ? "Creating branch" : "Create branch"}:{" "}
              <strong>{candidate}</strong> from &apos;{label}&apos;
            </span>
          </button>
        )}
        {createError && (
          <p className="revision-picker-error" role="alert">
            {createError}
          </p>
        )}
      </PopoverContent>
    </Popover>
  );
}
