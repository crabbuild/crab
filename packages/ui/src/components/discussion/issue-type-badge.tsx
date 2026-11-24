import { Badge } from "@/components/ui/badge";
import type { IssueType } from "@/lib/contracts";

const colors: Record<IssueType["color"], string> = {
  gray: "bg-gray-500",
  blue: "bg-blue-500",
  green: "bg-green-500",
  yellow: "bg-yellow-500",
  orange: "bg-orange-500",
  red: "bg-red-500",
  purple: "bg-purple-500",
  pink: "bg-pink-500",
};

export function IssueTypeBadge({ type }: { type: IssueType }) {
  return (
    <Badge variant="outline" title={type.description}>
      <span
        className={`size-2 shrink-0 rounded-full ${colors[type.color]}`}
        aria-hidden="true"
      />
      <span className="truncate">{type.name}</span>
    </Badge>
  );
}
