import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Input } from "@/components/ui/input";
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from "@/components/ui/popover";
import { ChevronDown, Puzzle, X } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
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
    <Popover open={open} onOpenChange={setOpen}>
      <div className="flex flex-wrap items-center gap-1 rounded border border-border px-2 py-1">
        {selectedSkills.map((skill) => (
          <span
            key={skill.record.id}
            className="inline-flex items-center gap-1 rounded bg-accent px-2 py-0.5 text-xs"
          >
            <Puzzle size={10} />
            {skill.frontmatter.name || skill.record.name}
            <Button
              variant="ghost"
              size="compact"
              className="p-0.5"
              aria-label={`Remove ${skill.record.name}`}
              onClick={() => remove(skill.record.id)}
            >
              <X size={10} />
            </Button>
          </span>
        ))}
        <PopoverTrigger asChild>
          <Button
            variant="ghost"
            size="compact"
            className="ml-auto"
            aria-label="Select skills"
          >
            {selectedSkills.length ? "Add skills" : "Select skills…"}
        <ChevronDown size={12} />
          </Button>
        </PopoverTrigger>
      </div>
      <PopoverContent align="start" className="w-80 p-0">
        <div className="border-b p-2">
          <Input
            aria-label="Search skills"
            placeholder="Search skills…"
            value={search}
            onChange={(event) => setSearch(event.target.value)}
          />
        </div>
        <div className="max-h-48 overflow-auto p-1">
            {loading ? (
              <p className="p-3 text-xs text-muted-foreground">Loading…</p>
            ) : filtered.length === 0 ? (
              <p className="p-3 text-xs text-muted-foreground">
                {skills.length ? "No matches" : "No skills available"}
            </p>
            ) : (
              filtered.map((skill) => (
              <label
                key={skill.record.id}
                className="flex cursor-pointer items-center gap-2 rounded px-2 py-1.5 text-xs hover:bg-accent focus-within:bg-accent"
              >
                <Checkbox
                  checked={selected.includes(skill.record.id)}
                  onCheckedChange={() => toggle(skill.record.id)}
                />
                    <span className="min-w-0 flex-1 truncate">
                  {skill.frontmatter.name || skill.record.name}
                    </span>
                    <span className="max-w-32 truncate text-[10px] text-muted-foreground">
                  {skill.record.description}
                    </span>
                  </label>
            ))
            )}
          </div>
        </PopoverContent>
    </Popover>
  );
}
