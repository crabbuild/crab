import { useState } from "react";
import { IssueTypeBadge } from "@/components/discussion/issue-type-badge";
import { Button } from "@/components/ui/button";
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
