import { useState, useMemo } from "react";
import { useSkills } from "../../state/skills-store";

function ListSkeleton() {
  return (
    <div className="w-64 shrink-0 border-r border-gx-border flex flex-col animate-pulse">
      <div className="p-2">
        <div className="h-7 bg-gx-border rounded w-full" />
      </div>
      <div className="flex-1 space-y-px">
        {Array.from({ length: 5 }).map((_, i) => (
          <div key={i} className="px-3 py-2 border-b border-gx-border/50">
            <div className="flex items-center gap-2">
              <div className="h-2 w-2 rounded-full bg-gx-border shrink-0" />
              <div className="h-4 bg-gx-border rounded w-28" />
            </div>
            <div className="h-3 bg-gx-border rounded w-44 mt-1.5" />
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
    <div className="w-64 shrink-0 border-r border-gx-border flex flex-col">
      <div className="p-2">
        <input
          type="text"
          placeholder="Search skills..."
          value={search}
          onChange={(e) => setSearch(e.target.value)}
          className="w-full rounded border border-gx-border bg-gx-surface-hover px-2 py-1 text-xs text-gx-text placeholder:text-gx-muted focus:outline-none focus:border-gx-accent"
        />
      </div>

      <div className="flex-1 overflow-y-auto">
        {filtered.length === 0 ? (
          <div className="p-4 text-center text-xs text-gx-muted">
            {state.skills.length === 0
              ? "No skills yet. Create one to get started."
              : "No skills match your search."}
          </div>
        ) : (
          filtered.map((skill) => (
            <button
              key={skill.record.id}
              className={`w-full text-left px-3 py-2 border-b border-gx-border/50 hover:bg-gx-surface-hover transition-colors ${
                state.selectedSkillId === skill.record.id ? "bg-gx-accent/10 border-l-2 border-l-gx-accent" : ""
              }`}
              onClick={() => selectSkill(skill.record.id)}
            >
              <div className="flex items-center gap-2">
                <span
                  className={`h-2 w-2 rounded-full shrink-0 ${
                    skill.record.enabled ? "bg-green-500" : "bg-gx-muted"
                  }`}
                />
                <span className="text-xs font-medium text-gx-text truncate">
                  {skill.frontmatter.name || skill.record.name}
                </span>
              </div>
              <p className="text-[11px] text-gx-muted mt-0.5 truncate">
                {skill.record.description}
              </p>
            </button>
          ))
        )}
      </div>
    </div>
  );
}
