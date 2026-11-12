import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { useMemo, useState } from "react";
import { useSkills } from "../../state/skills-store";

function ListSkeleton() {
  return (
    <div className="w-64 shrink-0 border-r border-border flex flex-col animate-pulse">
      <div className="p-2">
        <div className="h-7 bg-border rounded w-full" />
      </div>
      <div className="flex-1 space-y-px">
        {Array.from({ length: 5 }).map((_, i) => (
          <div key={i} className="px-3 py-2 border-b border-border/50">
            <div className="flex items-center gap-2">
              <div className="h-2 w-2 rounded-full bg-border shrink-0" />
              <div className="h-4 bg-border rounded w-28" />
            </div>
            <div className="h-3 bg-border rounded w-44 mt-1.5" />
          </div>
        ))}
      </div>
    </div>
  );
}

export function SkillList() {
  const { state, selectSkill } = useSkills();
  const [search, setSearch] = useState("");

  const filtered = useMemo(() => {
    if (!search.trim()) return state.skills;
    const q = search.toLowerCase();
    return state.skills.filter(
      (s) =>
        s.record.name.toLowerCase().includes(q) ||
        s.record.description.toLowerCase().includes(q),
    );
  }, [state.skills, search]);

  if (state.loading) {
    return <ListSkeleton />;
  }

  return (
    <div className="w-64 shrink-0 border-r border-border flex flex-col">
      <div className="p-2">
        <Input
          type="text"
          placeholder="Search skills..."
          value={search}
          onChange={(e) => setSearch(e.target.value)}
          className="w-full rounded border border-border bg-accent px-2 py-1 text-xs text-foreground placeholder:text-muted-foreground focus:outline-none focus:border-primary"
        />
      </div>

      <div className="flex-1 overflow-y-auto">
        {filtered.length === 0 ? (
          <div className="p-4 text-center text-xs text-muted-foreground">
            {state.skills.length === 0
              ? "No skills yet. Create one to get started."
              : "No skills match your search."}
          </div>
        ) : (
          filtered.map((skill) => (
            <Button
              variant="ghost"
              size="compact"
              key={skill.record.id}
              className={`w-full text-left px-3 py-2 border-b border-border/50 hover:bg-accent transition-colors ${
                state.selectedSkillId === skill.record.id ? "bg-primary/10 border-l-2 border-l-primary" : ""
              }`}
              onClick={() => selectSkill(skill.record.id)}
            >
              <div className="flex items-center gap-2">
                <span
                  className={`h-2 w-2 rounded-full shrink-0 ${
                    skill.record.enabled ? "bg-success" : "bg-muted-foreground"
                  }`}
                />
                <span className="text-xs font-medium text-foreground truncate">
                  {skill.frontmatter.name || skill.record.name}
                </span>
              </div>
              <p className="text-[11px] text-muted-foreground mt-0.5 truncate">
                {skill.record.description}
              </p>
            </Button>
          ))
        )}
      </div>
    </div>
  );
}
