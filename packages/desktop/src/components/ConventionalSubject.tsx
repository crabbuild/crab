import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Toggle } from "@/components/ui/toggle";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

// Standard conventional-commit types.
const COMMIT_TYPES = [
  "feat",
  "fix",
  "docs",
  "style",
  "refactor",
  "perf",
  "test",
  "build",
  "ci",
  "chore",
  "revert",
] as const;

type CommitType = (typeof COMMIT_TYPES)[number];

interface ParsedSubject {
  type: CommitType;
  scope: string;
  breaking: boolean;
  description: string;
}

/**
 * Parse an existing conventional-commit subject string back into its
 * structured parts. Returns `null` if the string doesn't match the
 * conventional-commit format.
 */
export function parseConventionalSubject(subject: string): ParsedSubject | null {
  // Pattern: type(scope)!: description  OR  type!: description  OR  type: description
  const match = subject.match(
    /^(\w+)(?:\(([^)]*)\))?(!)?\s*:\s*(.*)$/,
  );
  if (!match) return null;

  const [, rawType, scope, bang, description] = match;
  const type = rawType as CommitType;
  if (!COMMIT_TYPES.includes(type)) return null;

  return {
    type,
    scope: scope ?? "",
    breaking: bang === "!",
    description: description ?? "",
  };
}

/**
 * Compose the final subject string from structured parts.
 */
function composeSubject(parts: ParsedSubject): string {
  const { type, scope, breaking, description } = parts;
  const scopePart = scope ? `(${scope})` : "";
  const bangPart = breaking ? "!" : "";
  return `${type}${scopePart}${bangPart}: ${description}`;
}

interface Props {
  /** The current composed subject string. */
  value: string;
  /** Called with the new composed subject whenever any part changes. */
  onChange: (subject: string) => void;
  /** Keyboard handler forwarded to all inputs (for Ctrl/Cmd+Enter). */
  onKeyDown?: (e: React.KeyboardEvent) => void;
}

/**
 * Structured conventional-commit subject input. Replaces the plain
 * subject `<input>` in CommitComposer when the setting is enabled.
 *
 * Composes `type(scope)!: description` from individual controls and
 * exposes the final string via `onChange`.
 */
export function ConventionalSubject({ value, onChange, onKeyDown }: Props) {
  // Parse the incoming value into structured parts. If it doesn't
  // parse as conventional-commit format, use defaults with the raw
  // value as the description.
  const initial = useMemo(() => {
    const parsed = parseConventionalSubject(value);
    if (parsed) return parsed;
    return { type: "feat" as CommitType, scope: "", breaking: false, description: value };
  }, []); // Only compute on mount — after that, local state drives.

  const [type, setType] = useState<CommitType>(initial.type);
  const [scope, setScope] = useState(initial.scope);
  const [breaking, setBreaking] = useState(initial.breaking);
  const [description, setDescription] = useState(initial.description);

  const descRef = useRef<HTMLInputElement>(null);

  // Re-sync local state when the parent value changes externally
  // (e.g. sessionStorage restore on repo switch).
  const lastEmitted = useRef(value);
  useEffect(() => {
    if (value === lastEmitted.current) return;
    const parsed = parseConventionalSubject(value);
    if (parsed) {
      setType(parsed.type);
      setScope(parsed.scope);
      setBreaking(parsed.breaking);
      setDescription(parsed.description);
    } else {
      setDescription(value);
    }
    lastEmitted.current = value;
  }, [value]);

  // Emit the composed string whenever any part changes.
  const emit = useCallback(
    (t: CommitType, s: string, b: boolean, d: string) => {
      const composed = composeSubject({ type: t, scope: s, breaking: b, description: d });
      lastEmitted.current = composed;
      onChange(composed);
    },
    [onChange],
  );

  const handleTypeChange = useCallback(
    (selectedValue: string) => {
      const next = selectedValue.slice(1) as CommitType;
      setType(next);
      emit(next, scope, breaking, description);
    },
    [scope, breaking, description, emit],
  );

  const handleScopeChange = useCallback(
    (e: React.ChangeEvent<HTMLInputElement>) => {
      const next = e.target.value;
      setScope(next);
      emit(type, next, breaking, description);
    },
    [type, breaking, description, emit],
  );

  const handleBreakingToggle = useCallback(() => {
    const next = !breaking;
    setBreaking(next);
    emit(type, scope, next, description);
  }, [type, scope, breaking, description, emit]);

  const handleDescriptionChange = useCallback(
    (e: React.ChangeEvent<HTMLInputElement>) => {
      const next = e.target.value;
      setDescription(next);
      emit(type, scope, breaking, next);
    },
    [type, scope, breaking, emit],
  );

  return (
    <div className="flex w-full items-center gap-1" onKeyDown={onKeyDown}>
      {/* Type dropdown */}
      <Select value={":" + String(type)} onValueChange={handleTypeChange}>
        <SelectTrigger
          className="shrink-0 rounded border border-border bg-background px-1.5 py-1 text-xs text-foreground focus:border-primary focus-visible:outline-none"
        aria-label="Commit type"
      >
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          <SelectGroup>
            {COMMIT_TYPES.map((t) => (
          <SelectItem key={t} value={":" + String(t)}>
            {t}
          </SelectItem>
        ))}
      </SelectGroup>
        </SelectContent>
      </Select>

      {/* Scope input */}
      <Input
        type="text"
        value={scope}
        onChange={handleScopeChange}
        onKeyDown={onKeyDown}
        placeholder="scope"
        className="w-16 shrink-0 rounded border border-border bg-background px-1.5 py-1 text-xs text-foreground placeholder:text-muted-foreground focus:border-primary focus-visible:outline-none"
        aria-label="Commit scope"
      />

      {/* Breaking change toggle */}
      <Toggle
        size="sm"
        type="button"
        onClick={handleBreakingToggle}
        title={breaking ? "Breaking change (click to unset)" : "Mark as breaking change"}
        className={`shrink-0 rounded border px-1.5 py-1 text-xs font-bold transition ${
          breaking
            ? "border-destructive/60 bg-destructive/20 text-destructive"
            : "border-border bg-background text-muted-foreground hover:text-foreground"
        }`}
        aria-label="Breaking change"
        pressed={breaking}
      >
        !
      </Toggle>

      {/* Description input */}
      <Input
        ref={descRef}
        type="text"
        value={description}
        onChange={handleDescriptionChange}
        onKeyDown={onKeyDown}
        placeholder="description"
        className="min-w-0 flex-1 rounded border border-border bg-background px-2 py-1 text-xs text-foreground placeholder:text-muted-foreground focus:border-primary focus-visible:outline-none"
        aria-label="Commit description"
      />
    </div>
  );
}
