import { FieldSelect } from "@/components/ui/field-select";
import { useState } from "react";
import { Link } from "@/components/navigation/link";
import { Button } from "@/components/ui/button";
import { Field, FieldLabel } from "@/components/ui/field";
import { repoHref } from "@/lib/repository-route";

interface PullTemplate {
  file: string;
  path: string;
  body: string;
}

export interface PullTemplates {
  default: PullTemplate | null;
  items: PullTemplate[];
  truncated: boolean;
  source: { id: string; owner: string; name: string; commit: string } | null;
}

export function PullTemplatePicker({
  catalog,
  initialTemplate,
  missingTemplate,
  body,
  onApply,
  disabled,
}: {
  catalog: PullTemplates;
  initialTemplate?: PullTemplate | null;
  missingTemplate: boolean;
  body: string;
  onApply: (body: string) => void;
  disabled: boolean;
}) {
  const [choice, setChoice] = useState<string>();
  const templates = catalog.default
    ? [catalog.default, ...catalog.items]
    : catalog.items;
  const selected = templates.find(
    (item) => item.path === (choice ?? initialTemplate?.path),
  );
  if (!templates.length && !missingTemplate && !catalog.truncated) return null;
  const source = catalog.source;
  return (
    <Field className="gap-2">
      {templates.length > 0 && (
        <>
          <FieldLabel htmlFor="pull-template">Pull request template</FieldLabel>
          <div className="flex flex-wrap items-center gap-2">
            <FieldSelect
              id="pull-template"
              value={selected?.path ?? ""}
              onValueChange={(nextValue) => setChoice(nextValue)}
              disabled={disabled}
              options={[
                { value: "", label: "Choose a template" },
                ...templates.map((item) => ({
                  value: item.path,
                  label:
                    item === catalog.default ? "Default template" : item.file,
                })),
              ]}
            />
            <Button
              type="button"
              size="sm"
              variant="outline"
              disabled={disabled || !selected || selected.body === body}
              onClick={() => selected && onApply(selected.body)}
            >
              {body ? "Replace description" : "Use template"}
            </Button>
          </div>
        </>
      )}
      {source && (
        <p className="muted text-sm">
          Shared templates from{" "}
          <Link
            href={repoHref(source, {
              rev: source.commit,
              repository_id: source.id,
            })}
          >
            {source.owner}/{source.name}
          </Link>
        </p>
      )}
      {(missingTemplate || catalog.truncated) && (
        <p className="muted text-sm" role="status">
          {missingTemplate
            ? "The requested pull request template is unavailable."
            : "Some named templates are outside the listing limit."}
        </p>
      )}
    </Field>
  );
}
