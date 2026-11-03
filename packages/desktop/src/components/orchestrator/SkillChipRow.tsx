import { useState, useEffect } from "react";
import { Puzzle, X } from "lucide-react";

interface Props {
  skills: string[];
  onRemove: (skillId: string) => void;
}

export function SkillChipRow({ skills, onRemove }: Props) {
  const [skillNames, setSkillNames] = useState<Map<string, string>>(new Map());

  useEffect(() => {
    let cancelled = false;
    const fetchNames = async () => {
      const unknownIds = skills.filter((id) => !skillNames.has(id));
      if (unknownIds.length === 0) return;

      const newNames = new Map(skillNames);
      for (const id of unknownIds) {
        try {
          const result = await window.crab.skills.getSkill(id);
          if (!cancelled && result.success && result.data) {
            newNames.set(id, result.data.frontmatter.name || result.data.record.name);
          } else {
            newNames.set(id, id);
          }
        } catch {
          newNames.set(id, id);
        }
      }
      if (!cancelled) setSkillNames(newNames);
    };
    fetchNames();
    return () => { cancelled = true; };
  }, [skills]);

  if (skills.length === 0) return null;

  return (
    <div className="flex flex-wrap gap-1">
      {skills.map((id) => (
        <span
          key={id}
          className="inline-flex items-center gap-1 rounded-full bg-gx-accent/10 px-2 py-0.5 text-[10px] text-gx-accent"
        >
          <Puzzle size={10} />
          {skillNames.get(id) ?? id}
          <button
            type="button"
            onClick={() => onRemove(id)}
            className="hover:text-gx-error transition-colors"
            title="Remove skill"
          >
            <X size={10} />
          </button>
        </span>
      ))}
    </div>
  );
}
