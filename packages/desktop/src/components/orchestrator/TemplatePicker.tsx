import clsx from "clsx";

// ── Types ───────────────────────────────────────────────────────────

export interface WorkspaceTemplate {
  name: string;
  description: string;
  icon?: string;
  agentType: "claude-code" | "codex" | "custom";
  model?: string;
  planMode: boolean;
  fastMode?: boolean;
  contextFiles?: string[];
  preferences?: string;
  pipeline?: string;
  autoFix?: boolean;
  budgetLimit?: number;
  baseBranch?: string;
  source: "builtin" | "repo" | "user";
}

export interface TemplatePickerProps {
  templates: WorkspaceTemplate[];
  selectedTemplate?: string;
  onSelect: (templateName: string) => void;
}

// ── Component ───────────────────────────────────────────────────────

/**
 * Grid of template cards for quick workspace creation.
 *
 * Renders a responsive 2–3 column grid. Each card shows the template
 * icon (emoji), name, and description. Clicking a card selects it
 * (highlighted with an accent border).
 *
 * Accessibility:
 *   - Each card is a `button` with `role="option"`.
 *   - The grid container uses `role="listbox"` with `aria-label`.
 *   - Selected card has `aria-selected="true"`.
 */
export function TemplatePicker({ templates, selectedTemplate, onSelect }: TemplatePickerProps) {
  if (templates.length === 0) {
    return (
      <p className="text-xs text-muted-foreground">No templates available.</p>
    );
  }

  return (
    <div
      role="listbox"
      aria-label="Workspace templates"
      className="grid grid-cols-2 gap-2 sm:grid-cols-3"
    >
      {templates.map((template) => {
        const isSelected = selectedTemplate === template.name;

        return (
          <button
            key={template.name}
            type="button"
            role="option"
            aria-selected={isSelected}
            onClick={() => onSelect(template.name)}
            className={clsx(
              "flex flex-col items-start gap-1 rounded-lg border p-3 text-left transition",
              "hover:bg-accent focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary",
              isSelected
                ? "border-primary bg-primary/10"
                : "border-border bg-background",
            )}
          >
            {template.icon && (
              <span className="text-lg leading-none" aria-hidden="true">
                {template.icon}
              </span>
            )}
            <span className="text-xs font-medium text-foreground">
              {template.name}
            </span>
            {template.description && (
              <span className="text-[11px] leading-snug text-muted-foreground line-clamp-2">
                {template.description}
              </span>
            )}
          </button>
        );
      })}
    </div>
  );
}
