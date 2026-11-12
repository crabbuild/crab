import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { RadioGroup, RadioGroupItem } from "@/components/ui/radio-group";
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group";
import { Boxes, FileType, Filter, Folder, ListChecks, PackageOpen } from "lucide-react";
import { useEffect, useMemo } from "react";
import {
  PrefetchProfile,
  TrackedPattern,
} from "../../hooks/useTrackedPatterns";
import { HydrationEntry } from "../../ipc";
import { formatBytes } from "../../lib/format";
import {
  Scope,
  bucketByExtension,
  bucketByPattern,
  bucketByTopFolder,
} from "../../lib/hydrateScope";
import { CustomFilePicker } from "./CustomFilePicker";

type Mode = Scope["kind"];

interface Tab {
  id: Mode;
  label: string;
  icon: typeof Filter;
  /** Hidden when the repo has nothing to show for this mode. */
  available: boolean;
}

interface Props {
  scope: Scope;
  onChange: (s: Scope) => void;
  entries: HydrationEntry[];
  trackedPatterns: TrackedPattern[];
  profiles: PrefetchProfile[];
}

/**
 * Scope picker for the Hydrate tab. Replaces the freeform pattern
 * textarea with five structured modes — All, Pattern, Extension,
 * Folder, Custom — plus an optional Profile mode when a
 * `.crab/prefetch.toml` is present. Each mode resolves to a concrete
 * file set against the live hydration status, so the user always
 * sees an honest preview before kicking off a job.
 */
export function HydrateScopePicker({
  scope,
  onChange,
  entries,
  trackedPatterns,
  profiles,
}: Props) {
  const tabs: Tab[] = [
    { id: "all", label: "All", icon: Boxes, available: true },
    {
      id: "patterns",
      label: "Pattern",
      icon: Filter,
      available: trackedPatterns.length > 0,
    },
    { id: "extensions", label: "Type", icon: FileType, available: entries.length > 0 },
    { id: "folders", label: "Folder", icon: Folder, available: entries.length > 0 },
    { id: "custom", label: "Custom", icon: ListChecks, available: entries.length > 0 },
    {
      id: "profile",
      label: "Profile",
      icon: PackageOpen,
      available: profiles.length > 0,
    },
  ];

  const visibleTabs = tabs.filter((t) => t.available || t.id === "all");

  return (
    <div className="space-y-3">
      <ToggleGroup
        type="single"
        value={scope.kind}
        onValueChange={(value) => {
          if (value)
            onChange(
              defaultScopeFor(
                value as Parameters<typeof defaultScopeFor>[0],
                trackedPatterns,
                profiles,
              ),
            );
        }}
        aria-label="Hydrate scope"
        className="inline-flex flex-wrap items-center gap-px rounded-md border border-border bg-card p-0.5"
      >
        {visibleTabs.map((t) => {
          const active = scope.kind === t.id;
          const Icon = t.icon;
          return (
            <ToggleGroupItem
              value={t.id}
              key={t.id}
              className={
                "inline-flex items-center gap-1.5 rounded px-2.5 py-1 text-xs transition " +
                (active
                  ? "bg-background text-foreground"
                  : "text-muted-foreground hover:text-foreground")
              }
            >
              <Icon size={12} />
              {t.label}
            </ToggleGroupItem>
          );
        })}
      </ToggleGroup>

      <div className="rounded border border-border bg-background p-3">
        {scope.kind === "all" && (
          <p className="text-xs text-muted-foreground">
            All Crab and Git LFS files in the repository.
          </p>
        )}

        {scope.kind === "patterns" && (
          <PatternList
            patterns={trackedPatterns}
            entries={entries}
            selected={scope.patterns}
            onChange={(patterns) => onChange({ kind: "patterns", patterns })}
          />
        )}

        {scope.kind === "extensions" && (
          <ExtensionList
            entries={entries}
            selected={scope.extensions}
            onChange={(extensions) => onChange({ kind: "extensions", extensions })}
          />
        )}

        {scope.kind === "folders" && (
          <FolderList
            entries={entries}
            selected={scope.folders}
            onChange={(folders) => onChange({ kind: "folders", folders })}
          />
        )}

        {scope.kind === "custom" && (
          <CustomFilePicker
            entries={entries}
            selected={scope.paths}
            onChange={(paths) => onChange({ kind: "custom", paths })}
          />
        )}

        {scope.kind === "profile" && (
          <ProfileList
            profiles={profiles}
            entries={entries}
            selected={scope.name}
            onChange={(profile) =>
              onChange({
                kind: "profile",
                name: profile.name,
                patterns: profile.patterns,
              })
            }
          />
        )}
      </div>
    </div>
  );
}

/**
 * Pick a sensible default scope payload when the user clicks a tab.
 * For pattern/extension/folder modes we don't pre-select anything —
 * forces the user to make an explicit choice and avoids accidentally
 * inheriting state from another mode.
 */
function defaultScopeFor(
  mode: Mode,
  trackedPatterns: TrackedPattern[],
  profiles: PrefetchProfile[],
): Scope {
  switch (mode) {
    case "all":
      return { kind: "all" };
    case "patterns":
      return { kind: "patterns", patterns: trackedPatterns.map((p) => p.pattern) };
    case "extensions":
      return { kind: "extensions", extensions: [] };
    case "folders":
      return { kind: "folders", folders: [] };
    case "custom":
      return { kind: "custom", paths: [] };
    case "profile": {
      const first = profiles[0];
      return first
        ? { kind: "profile", name: first.name, patterns: first.patterns }
        : { kind: "all" };
    }
  }
}

// ── Pattern list ────────────────────────────────────────────────────

function PatternList({
  patterns,
  entries,
  selected,
  onChange,
}: {
  patterns: TrackedPattern[];
  entries: HydrationEntry[];
  selected: string[];
  onChange: (next: string[]) => void;
}) {
  const buckets = useMemo(
    () => bucketByPattern(patterns.map((p) => p.pattern), entries),
    [patterns, entries],
  );
  const selSet = useMemo(() => new Set(selected), [selected]);

  return (
    <div>
      <div className="mb-2 flex items-center justify-between text-[11px] uppercase tracking-wider text-muted-foreground">
        <span>Tracked patterns from .gitattributes</span>
        <SelectAllToggle
          allSelected={selSet.size === buckets.length && buckets.length > 0}
          onToggle={(all) => onChange(all ? buckets.map((b) => b.pattern) : [])}
        />
      </div>
      <ul className="space-y-px">
        {buckets.map((b) => {
          const checked = selSet.has(b.pattern);
          const empty = b.count === 0;
          return (
            <li key={b.pattern}>
              <label
                className={
                  "flex cursor-pointer items-center justify-between rounded px-2 py-1.5 text-xs transition " +
                  (checked
                    ? "bg-primary/10 text-foreground"
                    : empty
                      ? "text-muted-foreground/60 hover:bg-background/50"
                      : "hover:bg-background")
                }
                title={empty ? "No tracked files match this pattern yet" : undefined}
              >
                <span className="flex items-center gap-2">
                  <Checkbox
                    className="accent-primary"
                    checked={checked}
                    onCheckedChange={(checked) => {
                      const next = new Set(selSet);
                      if (checked === true) next.add(b.pattern);
                      else next.delete(b.pattern);
                      onChange(Array.from(next));
                    }}
                  />
                  <code className="font-mono">{b.pattern}</code>
                </span>
                <span className="text-muted-foreground">
                  {b.count.toLocaleString()} {b.count === 1 ? "file" : "files"}{" "}
                  · {formatBytes(b.bytes)}
                </span>
              </label>
            </li>
          );
        })}
      </ul>
    </div>
  );
}

// ── Extension list ──────────────────────────────────────────────────

function ExtensionList({
  entries,
  selected,
  onChange,
}: {
  entries: HydrationEntry[];
  selected: string[];
  onChange: (next: string[]) => void;
}) {
  const buckets = useMemo(() => bucketByExtension(entries), [entries]);
  const selSet = useMemo(() => new Set(selected.map((s) => s.toLowerCase())), [selected]);

  return (
    <div>
      <div className="mb-2 flex items-center justify-between text-[11px] uppercase tracking-wider text-muted-foreground">
        <span>Tracked file types</span>
        <SelectAllToggle
          allSelected={selSet.size === buckets.length && buckets.length > 0}
          onToggle={(all) => onChange(all ? buckets.map((b) => b.ext) : [])}
        />
      </div>
      <div className="flex flex-wrap gap-1.5">
        {buckets.map((b) => {
          const checked = selSet.has(b.ext);
          const label = b.ext === "" ? "(no extension)" : b.ext;
          return (
            <Button
              variant="ghost"
              size="compact"
              key={b.ext || "__none__"}
              type="button"
              className={
                "rounded-full border px-2.5 py-1 text-[11px] transition " +
                (checked
                  ? "border-primary bg-primary/15 text-foreground"
                  : "border-border text-muted-foreground hover:text-foreground")
              }
              onClick={() => {
                const next = new Set(selSet);
                if (checked) next.delete(b.ext);
                else next.add(b.ext);
                onChange(Array.from(next));
              }}
            >
              <code className="font-mono">{label}</code>
              <span className="ml-1.5 text-muted-foreground">
                {b.count} · {formatBytes(b.bytes)}
              </span>
            </Button>
          );
        })}
      </div>
    </div>
  );
}

// ── Folder list ─────────────────────────────────────────────────────

function FolderList({
  entries,
  selected,
  onChange,
}: {
  entries: HydrationEntry[];
  selected: string[];
  onChange: (next: string[]) => void;
}) {
  const buckets = useMemo(() => bucketByTopFolder(entries), [entries]);
  const selSet = useMemo(() => new Set(selected), [selected]);

  return (
    <div>
      <div className="mb-2 flex items-center justify-between text-[11px] uppercase tracking-wider text-muted-foreground">
        <span>Top-level folders containing tracked files</span>
        <SelectAllToggle
          allSelected={selSet.size === buckets.length && buckets.length > 0}
          onToggle={(all) => onChange(all ? buckets.map((b) => b.folder) : [])}
        />
      </div>
      <ul className="space-y-px">
        {buckets.map((b) => {
          const checked = selSet.has(b.folder);
          const label = b.folder === "" ? "(repo root)" : b.folder + "/";
          return (
            <li key={b.folder || "__root__"}>
              <label
                className={
                  "flex cursor-pointer items-center justify-between rounded px-2 py-1.5 text-xs transition " +
                  (checked ? "bg-primary/10 text-foreground" : "hover:bg-background")
                }
              >
                <span className="flex items-center gap-2">
                  <Checkbox
                    className="accent-primary"
                    checked={checked}
                    onCheckedChange={(checked) => {
                      const next = new Set(selSet);
                      if (checked === true) next.add(b.folder);
                      else next.delete(b.folder);
                      onChange(Array.from(next));
                    }}
                  />
                  <code className="font-mono">{label}</code>
                </span>
                <span className="text-muted-foreground">
                  {b.count.toLocaleString()} {b.count === 1 ? "file" : "files"}{" "}
                  · {formatBytes(b.bytes)}
                </span>
              </label>
            </li>
          );
        })}
      </ul>
    </div>
  );
}

// ── Profile list ────────────────────────────────────────────────────

function ProfileList({
  profiles,
  entries,
  selected,
  onChange,
}: {
  profiles: PrefetchProfile[];
  entries: HydrationEntry[];
  selected: string;
  onChange: (profile: PrefetchProfile) => void;
}) {
  // Compute file/byte counts per profile for the preview chip.
  const stats = useMemo(() => {
    const out = new Map<string, { count: number; bytes: number }>();
    for (const p of profiles) {
      const buckets = bucketByPattern(p.patterns, entries);
      const count = buckets.reduce((s, b) => s + b.count, 0);
      const bytes = buckets.reduce((s, b) => s + b.bytes, 0);
      out.set(p.name, { count, bytes });
    }
    return out;
  }, [profiles, entries]);

  // Auto-select the first profile if nothing is currently selected
  // (or the selection refers to a profile that no longer exists).
  // Keeps the picker from showing an empty preview on first open
  // even after the prefetch.toml is hot-reloaded.
  useEffect(() => {
    const stillValid = selected && profiles.some((p) => p.name === selected);
    if (!stillValid && profiles[0]) onChange(profiles[0]);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [profiles[0]?.name, selected]);

  return (
    <div>
      <div className="mb-2 text-[11px] uppercase tracking-wider text-muted-foreground">
        Saved profiles from .crab/prefetch.toml
      </div>
      <RadioGroup
        aria-label="Hydration profile"
        value={selected}
        onValueChange={(value) => {
          const profile = profiles.find((profile) => profile.name === value);
          if (profile) onChange(profile);
        }}
        className="space-y-px">
        {profiles.map((p) => {
          const active = selected === p.name;
          const s = stats.get(p.name);
          return (
            <div key={p.name}>
              <label
                className={
                  "flex w-full items-center justify-between rounded px-2 py-1.5 text-left text-xs transition " +
                  (active ? "bg-primary/10 text-foreground" : "hover:bg-background")
                }
              >
                <span className="flex items-center gap-2">
                  <RadioGroupItem value={p.name} />
                  <code className="font-mono">{p.name}</code>
                  <span className="text-muted-foreground">
                    {p.patterns.length} pattern
                    {p.patterns.length === 1 ? "" : "s"}
                  </span>
                </span>
                <span className="text-muted-foreground">
                  {s ? `${s.count.toLocaleString()} files · ${formatBytes(s.bytes)}` : ""}
                </span>
              </label>
            </div>
          );
        })}
      </RadioGroup>
    </div>
  );
}

// ── Shared bits ─────────────────────────────────────────────────────

function SelectAllToggle({
  allSelected,
  onToggle,
}: {
  allSelected: boolean;
  onToggle: (all: boolean) => void;
}) {
  return (
    <Button
      variant="ghost"
      size="compact"
      type="button"
      className="text-[11px] text-primary hover:underline"
      onClick={() => onToggle(!allSelected)}
    >
      {allSelected ? "Clear" : "Select all"}
    </Button>
  );
}

export type { Mode };
