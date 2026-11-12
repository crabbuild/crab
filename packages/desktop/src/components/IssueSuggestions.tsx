import {
  CommandGroup,
  CommandItem,
  CommandList,
  CommandPopover,
} from "@/components/ui/command";
import { useCallback, useEffect, useRef, useState } from "react";
import { ForgeIssue, rpc } from "../ipc";

interface Props {
  /** The repo to fetch issues from. */
  repoId: string;
  /** Current text value of the input/textarea. */
  value: string;
  /** Current cursor position (selectionStart) in the text field. */
  cursorPosition: number;
  /** Called when the user selects an issue — provides the new text value. */
  onInsert: (newValue: string) => void;
  /** Anchor element ref for positioning the dropdown. */
  anchorRef: React.RefObject<HTMLElement | null>;
}

/**
 * Monitors a text field for `#` followed by optional digits/text and
 * shows a dropdown of matching issues fetched from the forge API.
 * Debounced at 300ms to avoid excessive API calls.
 */
export function IssueSuggestions({
  repoId,
  value,
  cursorPosition,
  onInsert,
  anchorRef,
}: Props) {
  const [issues, setIssues] = useState<ForgeIssue[]>([]);
  const [visible, setVisible] = useState(false);
  const [selectedIndex, setSelectedIndex] = useState(0);
  const [loading, setLoading] = useState(false);
  const debounceRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const containerRef = useRef<HTMLDivElement>(null);

  // Extract the `#query` token at the cursor position.
  const getHashToken = useCallback((): { query: string; start: number; end: number } | null => {
    if (cursorPosition <= 0) return null;
    // Walk backwards from cursor to find `#`
    const textBefore = value.slice(0, cursorPosition);
    const match = textBefore.match(/#([a-zA-Z0-9_-]*)$/);
    if (!match) return null;
    const start = textBefore.length - match[0].length;
    return { query: match[1], start, end: cursorPosition };
  }, [value, cursorPosition]);

  // Fetch issues when the hash token changes.
  useEffect(() => {
    const token = getHashToken();
    if (!token) {
      setVisible(false);
      setIssues([]);
      return;
    }

    setVisible(true);
    setSelectedIndex(0);

    if (debounceRef.current) {
      clearTimeout(debounceRef.current);
    }

    debounceRef.current = setTimeout(() => {
      setLoading(true);
      rpc
        .forgeListIssues(repoId, token.query || undefined)
        .then((result) => {
          setIssues(result.issues);
          setSelectedIndex(0);
        })
        .catch(() => {
          setIssues([]);
        })
        .finally(() => {
          setLoading(false);
        });
    }, 300);

    return () => {
      if (debounceRef.current) {
        clearTimeout(debounceRef.current);
      }
    };
  }, [repoId, value, cursorPosition, getHashToken]);

  // Insert the selected issue number into the text.
  const insertIssue = useCallback(
    (issue: ForgeIssue) => {
      const token = getHashToken();
      if (!token) return;
      const before = value.slice(0, token.start);
      const after = value.slice(token.end);
      const inserted = `#${issue.number}`;
      onInsert(before + inserted + after);
      setVisible(false);
      setIssues([]);
    },
    [value, getHashToken, onInsert],
  );

  // Keyboard navigation within the dropdown.
  const handleKeyDown = useCallback(
    (e: KeyboardEvent) => {
      if (!visible || issues.length === 0) return;

      if (e.key === "ArrowDown") {
        e.preventDefault();
        setSelectedIndex((i) => (i + 1) % issues.length);
      } else if (e.key === "ArrowUp") {
        e.preventDefault();
        setSelectedIndex((i) => (i - 1 + issues.length) % issues.length);
      } else if (e.key === "Enter" || e.key === "Tab") {
        e.preventDefault();
        insertIssue(issues[selectedIndex]);
      } else if (e.key === "Escape") {
        e.preventDefault();
        setVisible(false);
        setIssues([]);
      }
    },
    [visible, issues, selectedIndex, insertIssue],
  );

  // Attach keyboard listener to the anchor element.
  useEffect(() => {
    const el = anchorRef.current;
    if (!el || !visible) return;

    el.addEventListener("keydown", handleKeyDown as EventListener);
    return () => {
      el.removeEventListener("keydown", handleKeyDown as EventListener);
    };
  }, [anchorRef, visible, handleKeyDown]);

  // Dismiss on click outside.

  if (!visible) return null;

  return (
    <CommandPopover
      anchorRef={anchorRef}
      onClose={() => setVisible(false)}
      side="bottom"
      value={String(selectedIndex)}
      onValueChange={(value) => setSelectedIndex(Number(value))}
    >
      <CommandList ref={containerRef} className="max-h-48">
        <CommandGroup>
          {loading && issues.length === 0 && (
            <div className="px-3 py-2 text-[11px] text-muted-foreground">
              Loading issues…
            </div>
          )}
          {!loading && issues.length === 0 && (
            <div className="px-3 py-2 text-[11px] text-muted-foreground">
              No matching issues
            </div>
          )}
          {issues.map((issue, idx) => (
            <CommandItem
              value={String(idx)}
              key={issue.number}
              className={`flex w-full items-center gap-2 px-3 py-1.5 text-left text-[11px] transition ${
            idx === selectedIndex
              ? "bg-primary/20 text-foreground"
              : "text-foreground hover:bg-background"
          }`}
              onSelect={() => insertIssue(issue)}
            >
              <span className="shrink-0 font-mono text-primary">
                #{issue.number}
              </span>
              <span className="truncate">{issue.title}</span>
            </CommandItem>
          ))}
        </CommandGroup>
      </CommandList>
    </CommandPopover>
  );
}
