import { useState } from "react";
import { IssueTypeBadge } from "@/components/discussion/issue-type-badge";
import { Button } from "@/components/ui/button";
import { FieldSelect } from "@/components/ui/field-select";
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from "@/components/ui/popover";
import { IssueTypeControl } from "@/features/issues/type";
import type { Repository } from "@/lib/api";
import type { IssueType } from "@/lib/contracts";
import { useProjectCommands } from "./commands";
import type { Row } from "./model";

export function ProjectBoardGrouping({
  grouped,
  onChange,
  label = "Group board by",
  id,
}: {
  grouped: boolean;
  onChange: (grouped: boolean) => void;
  label?: string;
  id?: string;
}) {
  return (
    <FieldSelect
      id={id}
      label={label}
      value={grouped ? "type" : "status"}
      options={{ status: "Workflow status", type: "Issue type" }}
      onValueChange={(value) => onChange(value === "type")}
    />
  );
}

export function ProjectTypeFilter({
  types,
  value,
  onChange,
  label = "Filter by issue type",
  id,
}: {
  types: IssueType[];
  value: string;
  onChange: (value: string) => void;
  label?: string;
  id?: string;
}) {
  const options = {
    all: "All issue types",
    none: "No type",
    ...Object.fromEntries(types.map((type) => [type.id, type.name])),
  };
  return (
    <FieldSelect
      id={id}
      label={label}
      value={value || "all"}
      options={
        value && !Object.hasOwn(options, value)
          ? { ...options, [value]: "Unavailable type" }
          : options
      }
      onValueChange={(selected) => onChange(selected === "all" ? "" : selected)}
    />
  );
}

export function ProjectIssueType({
  row,
  repo,
  options,
  onChange,
}: {
  row: Row;
  repo?: Pick<Repository, "id" | "name" | "owner">;
  options: IssueType[];
  onChange?: () => void;
}) {
  const [open, setOpen] = useState(false);
  const { csrf } = useProjectCommands();
  const source = row.repository ?? repo;
  const work = row.work;
  const version = row.issue_type_version;
  if (row.restricted || work?.subject.kind !== "issue") return "—";
  if (!source || version == null || !options.length || !onChange)
    return row.issue_type ? <IssueTypeBadge type={row.issue_type} /> : "—";

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <Button
          type="button"
          variant="ghost"
          size="sm"
          aria-label={`Change issue type for ${work.title}`}
        >
          {row.issue_type ? (
            <IssueTypeBadge type={row.issue_type} />
          ) : (
            "Set type"
          )}
        </Button>
      </PopoverTrigger>
      {open && (
        <PopoverContent
          align="start"
          aria-label={`Issue type for ${work.title}`}
        >
          <IssueTypeControl
            repo={source}
            csrf={csrf}
            number={work.subject.number}
            version={version}
            type={row.issue_type}
            types={options}
            refresh={() => {
              setOpen(false);
              onChange();
            }}
          />
        </PopoverContent>
      )}
    </Popover>
  );
}
