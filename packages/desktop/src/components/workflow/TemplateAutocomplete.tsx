import { useCallback, useEffect, useRef, useState } from "react";
import clsx from "clsx";
import { rpc } from "../../ipc";
import type { TemplateCompletion } from "../../lib/workflow-types";

interface Props {
  value: string;
  onChange: (value: string) => void;
  repoId: string;
  workflowPath: string;
  placeholder?: string;
  className?: string;
}

export function TemplateAutocomplete({
  value,
  onChange,
  repoId,
  workflowPath,
  placeholder,
  className,
}: Props) {
  const inputRef = useRef<HTMLInputElement>(null);
  const [completions, setCompletions] = useState<TemplateCompletion[]>([]);
  const [showDropdown, setShowDropdown] = useState(false);
  const [selectedIndex, setSelectedIndex] = useState(0);
  const [cursorPos, setCursorPos] = useState(0);
  const [tooltip, setTooltip] = useState<{ text: string; x: number } | null>(null);
  const dropdownRef = useRef<HTMLDivElement>(null);

  // Detect `${` pattern and fetch completions.
  const handleInput = useCallback(
    async (e: React.ChangeEvent<HTMLInputElement>) => {
      const newValue = e.target.value;
      const pos = e.target.selectionStart ?? newValue.length;
      onChange(newValue);
      setCursorPos(pos);

      // Look backwards from cursor for an unclosed `${`.
      const before = newValue.slice(0, pos);
      const openIdx = before.lastIndexOf("${");
      if (openIdx === -1) {
        setShowDropdown(false);
        return;
      }

      // Check there's no closing `}` between `${` and cursor.
      const afterOpen = before.slice(openIdx + 2);
      if (afterOpen.includes("}")) {
        setShowDropdown(false);
        return;
      }

      const prefix = afterOpen;
      try {
        const { completions: results } = await rpc.workflowTemplateCompletions(
          repoId,
          prefix,
          workflowPath,
        );
        setCompletions(results);
        setSelectedIndex(0);
        setShowDropdown(results.length > 0);
      } catch {
        setShowDropdown(false);
      }
    },
    [onChange, repoId, workflowPath],
  );

  // Insert selected completion.
  const insertCompletion = useCallback(
    (completion: TemplateCompletion) => {
      const before = value.slice(0, cursorPos);
      const openIdx = before.lastIndexOf("${");
      if (openIdx === -1) return;

      const prefix = value.slice(0, openIdx);
      const suffix = value.slice(cursorPos);
      const inserted = `\${${completion.key}}`;
      const newValue = prefix + inserted + suffix;
      onChange(newValue);
      setShowDropdown(false);

      // Move cursor after the inserted expression.
      const newPos = prefix.length + inserted.length;
      requestAnimationFrame(() => {
        inputRef.current?.setSelectionRange(newPos, newPos);
        inputRef.current?.focus();
      });
    },
    [value, cursorPos, onChange],
  );

  // Keyboard navigation in dropdown.
  const handleKeyDown = useCallback(
    (e: React.KeyboardEvent) => {
      if (!showDropdown) return;

      if (e.key === "ArrowDown") {
        e.preventDefault();
        setSelectedIndex((i) => Math.min(i + 1, completions.length - 1));
      } else if (e.key === "ArrowUp") {
        e.preventDefault();
        setSelectedIndex((i) => Math.max(i - 1, 0));
      } else if (e.key === "Enter" || e.key === "Tab") {
        e.preventDefault();
        if (completions[selectedIndex]) {
          insertCompletion(completions[selectedIndex]);
        }
      } else if (e.key === "Escape") {
        setShowDropdown(false);
      }
    },
    [showDropdown, completions, selectedIndex, insertCompletion],
  );

  // Resolve tooltip on hover over `${...}` expressions.
  const handleMouseMove = useCallback(
    async (e: React.MouseEvent<HTMLInputElement>) => {
      const input = inputRef.current;
      if (!input) return;

      // Approximate character position from mouse x offset.
      const rect = input.getBoundingClientRect();
      const charWidth = 7.2; // approximate monospace char width at text-xs
      const offsetX = e.clientX - rect.left - 8; // padding offset
      const charPos = Math.floor(offsetX / charWidth);

      // Find if charPos is inside a `${...}` expression.
      const regex = /\$\{([^}]+)\}/g;
      let match: RegExpExecArray | null;
      let found = false;
      while ((match = regex.exec(value)) !== null) {
        const start = match.index;
        const end = start + match[0].length;
        if (charPos >= start && charPos < end) {
          found = true;
          try {
            const { resolved } = await rpc.workflowResolveTemplate(
              repoId,
              match[0],
              workflowPath,
            );
            setTooltip({ text: resolved, x: e.clientX - rect.left });
          } catch {
            setTooltip({ text: "(unresolved)", x: e.clientX - rect.left });
          }
          break;
        }
      }
      if (!found) {
        setTooltip(null);
      }
    },
    [value, repoId, workflowPath],
  );

  const handleMouseLeave = useCallback(() => {
    setTooltip(null);
  }, []);

  // Close dropdown on outside click.
  useEffect(() => {
    const handler = (e: MouseEvent) => {
      if (
        dropdownRef.current &&
        !dropdownRef.current.contains(e.target as HTMLElement) &&
        !inputRef.current?.contains(e.target as HTMLElement)
      ) {
        setShowDropdown(false);
      }
    };
    document.addEventListener("mousedown", handler);
    return () => document.removeEventListener("mousedown", handler);
  }, []);

  return (
    <div className="relative">
      <input
        ref={inputRef}
        type="text"
        className={clsx("input-field w-full font-mono text-xs", className)}
        value={value}
        onChange={handleInput}
        onKeyDown={handleKeyDown}
        onMouseMove={handleMouseMove}
        onMouseLeave={handleMouseLeave}
        placeholder={placeholder}
      />

      {/* Tooltip for resolved values */}
      {tooltip && (
        <div
          className="absolute -top-7 z-50 rounded border border-gx-border bg-gx-surface px-2 py-0.5 text-[10px] text-gx-muted shadow"
          style={{ left: tooltip.x }}
        >
          {tooltip.text}
        </div>
      )}

      {/* Completions dropdown */}
      {showDropdown && (
        <div
          ref={dropdownRef}
          className="absolute left-0 top-full z-50 mt-1 max-h-48 w-full overflow-y-auto rounded border border-gx-border bg-gx-surface shadow-lg"
        >
          {completions.map((c, i) => (
            <button
              key={c.key}
              type="button"
              className={clsx(
                "flex w-full items-center gap-2 px-3 py-1.5 text-left text-xs",
                i === selectedIndex
                  ? "bg-gx-accent/10 text-gx-text"
                  : "text-gx-text hover:bg-gx-surface-hover",
              )}
              onMouseDown={(e) => {
                e.preventDefault();
                insertCompletion(c);
              }}
              onMouseEnter={() => setSelectedIndex(i)}
            >
              <span className="rounded bg-gx-bg px-1 py-0.5 text-[10px] text-gx-muted">
                {c.source}
              </span>
              <span className="flex-1 font-mono">{c.key}</span>
              <span className="truncate text-gx-muted">{c.value}</span>
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
