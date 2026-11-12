import { Folder } from "lucide-react";

/**
 * A small pill indicator shown next to a setting label when that
 * setting has a workspace-level override. Subtle — accent-colored
 * text with a folder icon, sized to not dominate the row.
 */
export function ScopeChip() {
  return (
    <span
      className="ml-2 inline-flex items-center gap-1 rounded-full bg-primary/10 px-2 py-0.5 text-[10px] font-medium text-primary"
      aria-label="This setting has a workspace override"
    >
      <Folder size={9} className="shrink-0" />
      Workspace
    </span>
  );
}
