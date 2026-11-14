import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Empty,
  EmptyHeader,
  EmptyTitle,
  EmptyDescription,
} from "@/components/ui/empty";
import { Input } from "@/components/ui/input";
import { cn } from "@/lib/utils";
import { useState } from "react";
import { useSkills } from "../../state/skills-store";

export function SkillList() {
  const { state, selectSkill } = useSkills();
  const [search, setSearch] = useState("");
  const query = search.trim().toLowerCase();
  const filtered = state.skills.filter((skill) =>
    `${skill.frontmatter.name} ${skill.record.name} ${skill.record.description}`
      .toLowerCase()
      .includes(query),
  );

  return (
    <aside
      aria-label="Skills library"
      className="flex w-64 min-w-0 shrink-0 flex-col border-r border-border max-[640px]:w-48"
    >
      <div className="space-y-2 p-3">
        <Input
          className="w-full"
          aria-label="Search skills"
          placeholder="Search skills…"
          value={search}
          onChange={(event) => setSearch(event.target.value)}
        />
        <p className="text-xs text-muted-foreground">
          {filtered.length} of {state.skills.length} skills
        </p>
      </div>
      <div className="flex-1 overflow-y-auto px-1 pb-2">
        {filtered.length === 0 ? (
          <Empty className="gap-3 px-3 py-8 md:p-3">
            <EmptyHeader>
              <EmptyTitle className="text-sm">No matching skills</EmptyTitle>
              <EmptyDescription className="text-xs">
                Try another name or description.
              </EmptyDescription>
            </EmptyHeader>
            <Button size="sm" variant="outline" onClick={() => setSearch("")}>
              Clear search
            </Button>
          </Empty>
        ) : (
          filtered.map((skill) => (
            <Button
              key={skill.record.id}
              variant="ghost"
              size="compact"
              aria-pressed={state.selectedSkillId === skill.record.id}
              className={cn(
                "mb-1 h-auto w-full flex-col items-start justify-start gap-1 px-3 py-3 text-left",
                state.selectedSkillId === skill.record.id &&
                  "bg-accent text-accent-foreground",
              )}
              onClick={() => selectSkill(skill.record.id)}
            >
              <span className="w-full truncate text-sm font-medium">
                {skill.frontmatter.name || skill.record.name}
              </span>
              <span className="line-clamp-2 w-full text-xs font-normal text-muted-foreground">
                {skill.record.description}
              </span>
              <span className="mt-1 flex flex-wrap gap-1">
                <Badge variant="secondary" className="text-[10px]">
                  {skill.record.scope === "user" ? "User" : "Repository"}
                </Badge>
                {!skill.record.enabled && (
                  <Badge variant="outline" className="text-[10px]">
                    Hidden from chat
                  </Badge>
                )}
              </span>
            </Button>
          ))
        )}
      </div>
    </aside>
  );
}
