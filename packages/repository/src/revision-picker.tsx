import { useId, useRef, useState } from "react";
import {
  ActionList,
  AnchoredOverlay,
  Button,
  IconButton,
  Label,
  TextInput,
} from "@primer/react";
import {
  GitBranchIcon,
  GitCommitIcon,
  SearchIcon,
  TagIcon,
  TriangleDownIcon,
  XIcon,
} from "@primer/octicons-react";
import type { Refs } from "./api";
import { short } from "./ui";

type RefType = "branches" | "tags";

export function RevisionPicker({
  refs,
  revision,
  onSelect,
}: {
  refs: Refs;
  revision: string;
  onSelect: (name: string) => void;
}) {
  const [open, setOpen] = useState(false);
  const [type, setType] = useState<RefType>("branches");
  const [filter, setFilter] = useState("");
  const input = useRef<HTMLInputElement>(null);
  const menu = useRef<HTMLUListElement>(null);
  const id = useId();
  const selected = refs.refs.find((ref) => ref.name === revision);
  const label = selected
    ? selected.name.replace(/^refs\/(heads|tags)\//, "")
    : short(revision);
  const icon = !selected
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
  return (
    <AnchoredOverlay
      open={open}
      onOpen={() => {
        setType(revision.startsWith("refs/tags/") ? "tags" : "branches");
        setFilter("");
        setOpen(true);
      }}
      onClose={() => setOpen(false)}
      width="medium"
      preventOverflow={false}
      displayCloseButton={false}
      focusZoneSettings={{ disabled: true }}
      focusTrapSettings={{ initialFocusRef: input }}
      overlayProps={{
        role: "dialog",
        "aria-labelledby": `${id}-title`,
        className: "revision-picker-panel",
      }}
      renderAnchor={(props) => (
        <Button
          {...props}
          leadingVisual={icon}
          trailingVisual={TriangleDownIcon}
          className="revision-picker-anchor"
          aria-label={`Switch branches or tags, current ${label}`}
          title={revision}
        >
          {label}
        </Button>
      )}
    >
      <header className="revision-picker-heading">
        <h2 id={`${id}-title`}>Switch branches/tags</h2>
        <IconButton
          icon={XIcon}
          aria-label="Close branch picker"
          variant="invisible"
          size="small"
          onClick={() => setOpen(false)}
        />
      </header>
      <div className="revision-picker-search">
        <TextInput
          ref={input}
          block
          leadingVisual={SearchIcon}
          aria-label={`Filter ${type}`}
          placeholder={`Find a ${type === "branches" ? "branch" : "tag"}…`}
          value={filter}
          onChange={(event) => setFilter(event.target.value)}
          onKeyDown={(event) => {
            if (event.key === "ArrowDown") {
              event.preventDefault();
              menu.current
                ?.querySelector<HTMLElement>('[role="menuitemradio"]')
                ?.focus();
            }
          }}
        />
      </div>
      <div
        className="revision-picker-tabs"
        role="tablist"
        aria-label="Ref type"
      >
        {(["branches", "tags"] as const).map((tab) => (
          <button
            key={tab}
            id={`${id}-${tab}`}
            role="tab"
            type="button"
            aria-selected={type === tab}
            aria-controls={`${id}-results`}
            tabIndex={type === tab ? 0 : -1}
            onClick={() => setType(tab)}
            onKeyDown={(event) => {
              if (
                ["ArrowLeft", "ArrowRight", "Home", "End"].includes(event.key)
              ) {
                event.preventDefault();
                const next =
                  event.key === "Home"
                    ? "branches"
                    : event.key === "End"
                      ? "tags"
                      : tab === "branches"
                        ? "tags"
                        : "branches";
                setType(next);
                document.getElementById(`${id}-${next}`)?.focus();
              }
            }}
          >
            {tab === "branches" ? "Branches" : "Tags"}
          </button>
        ))}
      </div>
      <div
        id={`${id}-results`}
        role="tabpanel"
        aria-labelledby={`${id}-${type}`}
        className="revision-picker-results"
      >
        {matches.length ? (
          <ActionList
            ref={menu}
            role="menu"
            aria-label={type === "branches" ? "Branches" : "Tags"}
            selectionVariant="single"
          >
            {matches.map((ref) => (
              <ActionList.Item
                key={ref.name}
                role="menuitemradio"
                selected={revision === ref.name}
                onSelect={() => {
                  setOpen(false);
                  onSelect(ref.name);
                }}
              >
                {ref.name.slice(prefix.length)}
                {refs.head?.name === ref.name && (
                  <ActionList.TrailingVisual>
                    <Label>default</Label>
                  </ActionList.TrailingVisual>
                )}
              </ActionList.Item>
            ))}
          </ActionList>
        ) : (
          <p className="revision-picker-empty" role="status">
            {filter ? `No ${type} match “${filter}”.` : `No ${type} yet.`}
          </p>
        )}
      </div>
    </AnchoredOverlay>
  );
}
