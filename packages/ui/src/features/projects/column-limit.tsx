import { useId, useState } from "react";
import { EllipsisIcon } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Field, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import {
  Popover,
  PopoverContent,
  PopoverDescription,
  PopoverTitle,
  PopoverTrigger,
} from "@/components/ui/popover";
import { useProjectCommands } from "./commands";
import type { Command, Project, ProjectTarget } from "./model";

type LimitCommand = Extract<Command, { kind: "column_limit" }>;

export function ColumnLimitControl({
  project,
  target,
  column,
  name,
  limit,
  editable,
}: {
  project: Project;
  target: ProjectTarget;
  column: Omit<LimitCommand["column"], "limit">;
  name: string;
  limit?: number;
  editable: boolean;
}) {
  const commands = useProjectCommands();
  const id = useId();
  const [draft, setDraft] = useState<{ limit?: number; version: number }>();

  function save(value: number | null) {
    if (!draft || !editable) return;
    const command: LimitCommand = {
      kind: "column_limit",
      project: project.id,
      version: draft.version,
      column: { ...column, limit: value },
    };
    setDraft(undefined);
    if ("repo" in target) void commands.submit(target.repo, command);
    else void commands.submitOrganization(target.organization, command);
  }

  return (
    <Popover
      open={!!draft}
      onOpenChange={(open) =>
        setDraft(open ? { limit, version: project.version } : undefined)
      }
    >
      <PopoverTrigger asChild>
        <Button
          type="button"
          size="icon-sm"
          variant="ghost"
          disabled={!editable}
          aria-label={`Column limit for ${name}`}
        >
          <EllipsisIcon />
        </Button>
      </PopoverTrigger>
      <PopoverContent aria-labelledby={`${id}-title`} align="end">
        <PopoverTitle id={`${id}-title`}>Column limit for {name}</PopoverTitle>
        <PopoverDescription>
          Warn when the item count exceeds this limit. Cards can still move into
          the column.
        </PopoverDescription>
        <form
          className="grid gap-3"
          onSubmit={(event) => {
            event.preventDefault();
            const value = new FormData(event.currentTarget).get("limit");
            save(Number(value));
          }}
        >
          <Field>
            <FieldLabel htmlFor={id}>Item limit</FieldLabel>
            <Input
              id={id}
              name="limit"
              type="number"
              min={1}
              max={4_294_967_295}
              step={1}
              required
              disabled={!editable}
              defaultValue={draft?.limit ?? ""}
            />
          </Field>
          <div className="flex flex-wrap justify-end gap-2">
            {draft?.limit !== undefined && (
              <Button
                type="button"
                size="sm"
                variant="ghost"
                disabled={!editable}
                onClick={() => save(null)}
              >
                Remove limit
              </Button>
            )}
            <Button type="submit" size="sm" disabled={!editable}>
              Save limit
            </Button>
          </div>
        </form>
      </PopoverContent>
    </Popover>
  );
}
