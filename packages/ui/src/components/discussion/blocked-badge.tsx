import { CircleMinusIcon } from "lucide-react";
import { Badge } from "@/components/ui/badge";

export function BlockedBadge() {
  return (
    <Badge variant="warning">
      <CircleMinusIcon aria-hidden="true" data-icon="inline-start" />
      Blocked
    </Badge>
  );
}
