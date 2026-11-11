import { Input } from "@/components/ui/input";
import { useState, useEffect, useRef, useMemo } from "react";
import { Puzzle, X, ChevronDown } from "lucide-react";
import type { SkillListItem } from "../../ipc";

interface Props {
  selected: string[];
  onChange: (ids: string[]) => void;
  repoPath?: string;
}

export function SkillPicker({ selected, onChange, repoPath }: Props) {
  const [skills, setSkills] = useState<SkillListItem[]>([]);
  const [loading, setLoading] = useState(true);
  const [search, setSearch] = useState("");
  const [open, setOpen] = useState(false);
  const containerRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    let cancelled = false;
    const load = async () => {
      try {
        const result = await window.crab.skills.listSkills(
          repoPath ? { repoPath } : undefined,
        );
        if (!cancelled && result.success && result.data) {
          setSkills(result.data.filter((s) => s.record.enabled));
        }
      } catch {
        // IPC not ready
      } finally {
        if (!cancelled) setLoading(false);
      }
    };
    load();
    return () => { cancelled = true; };
  }, [repoPath]);

  // Close on outside click
  useEffect(() => {
    if (!open) return;
    const handler = (e: MouseEvent) => {
      if (containerRef.current && !containerRef.current.contains(e.target as Node)) {
        setOpen(false);
      }
    };
    document.addEventListener("mousedown", handler);
    return () => document.removeEventListener("mousedown", handler);
  }, [open]);

  const filtered = useMemo(() => {
    if (!search.trim()) return skills;
    const q = search.toLowerCase();
    return skills.filter(
      (s) =>
        s.record.name.toLowerCase().includes(q) ||
        s.record.description.toLowerCase().includes(q),
    );
  }, [skills, search]);

  const toggle = (id: string) => {
    onChange(
      selected.includes(id)
        ? selected.filter((s) => s !== id)
        : [...selected, id],
    );
  };

  const remove = (id: string) => {
    onChange(selected.filter((s) => s !== id));
  };

  const selectedSkills = skills.filter((s) => selected.includes(s.record.id));

  return (
    <div ref={containerRef} className="relative">
      {/* Selected chips + open button */}
      <div
        className="flex flex-wrap items-center gap-1 min-h-[32px] rounded border border-border bg-accent px-2 py-1 cursor-pointer"
        onClick={() => { setOpen(!open); setTimeout(() => inputRef.current?.focus(), 50); }}
      >
        {selectedSkills.length === 0 && (
          <span className="text-xs text-muted-foreground">Select skills...</span>
        )}
        {selectedSkills.map((s) => (
          <span
            key={s.record.id}
            className="inline-flex items-center gap-1 rounded-full bg-primary/10 px-2 py-0.5 text-[10px] text-primary"
          >
            <Puzzle size={10} />
            {s.frontmatter.name || s.record.name}
            <button
              type="button"
              onClick={(e) => { e.stopPropagation(); remove(s.record.id); }}
              className="hover:text-destructive"
            >
              <X size={10} />
            </button>
          </span>
        ))}
        <ChevronDown size={14} className="ml-auto text-muted-foreground shrink-0" />
      </div>

      {/* Dropdown */}
      {open && (
        <div className="absolute top-full left-0 right-0 mt-1 z-50 rounded-lg border border-border bg-background shadow-xl max-h-48 overflow-hidden flex flex-col">
          <div className="p-1.5 border-b border-border">
            <Input
              ref={inputRef}
              type="text"
              placeholder="Search skills..."
              value={search}
              onChange={(e) => setSearch(e.target.value)}
              onClick={(e) => e.stopPropagation()}
              className="w-full rounded border border-border bg-accent px-2 py-1 text-xs text-foreground placeholder:text-muted-foreground focus:outline-none focus:border-primary"
            />
          </div>
          <div className="overflow-y-auto flex-1">
            {loading ? (
              <div className="p-3 text-xs text-muted-foreground text-center">Loading...</div>
            ) : filtered.length === 0 ? (
              <div className="p-3 text-xs text-muted-foreground text-center">
                {skills.length === 0 ? "No skills available" : "No matches"}
              </div>
            ) : (
              filtered.map((s) => {
                const isSelected = selected.includes(s.record.id);
                return (
                  <button
                    key={s.record.id}
                    type="button"
                    className={`w-full text-left px-3 py-1.5 flex items-center gap-2 text-xs hover:bg-accent transition-colors ${
                      isSelected ? "bg-primary/5" : ""
                    }`}
                    onClick={(e) => { e.stopPropagation(); toggle(s.record.id); }}
                  >
                    <input
                      type="checkbox"
                      checked={isSelected}
                      onChange={() => {}}
                      className="rounded"
                    />
                    <span className="text-foreground truncate">
                      {s.frontmatter.name || s.record.name}
                    </span>
                    <span className="text-muted-foreground truncate ml-auto text-[10px]">
                      {s.record.description.slice(0, 40)}
                    </span>
                  </button>
                );
              })
            )}
          </div>
        </div>
      )}
    </div>
  );
}
