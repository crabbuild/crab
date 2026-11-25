import { Checkbox } from "@/components/ui/checkbox";
import { FieldSelect } from "@/components/ui/field-select";
import {
  Field,
  FieldDescription,
  FieldLegend,
  FieldSet,
} from "@/components/ui/field";
import { Input } from "@/components/ui/input";

export type PropertyValue = boolean | string | string[];
export type PropertyOverrides = Record<string, PropertyValue>;
export type PropertyKind =
  | "text"
  | "boolean"
  | "single_select"
  | "multi_select";

export interface PropertyDefinition {
  id: string;
  name: string;
  description: string;
  kind: PropertyKind;
  options: string[];
  default: PropertyValue | null;
  required: boolean;
  require_explicit_values: boolean;
  values_editable_by: "organization" | "repository";
}

export interface PropertySchema {
  version: number;
  definitions: PropertyDefinition[];
}

export function propertySchemaPath(organization: string) {
  return `/api/organizations/${encodeURIComponent(organization)}/repository-properties`;
}

export const control =
  "w-full rounded-md border bg-background px-3 py-2 text-sm";

export function display(value: PropertyValue) {
  return Array.isArray(value) ? value.join(", ") : String(value);
}

export function needsExplicitPropertyValues(
  definitions: PropertyDefinition[],
  values: PropertyOverrides,
) {
  return definitions.some(
    (definition) =>
      definition.require_explicit_values && values[definition.id] == null,
  );
}

export function RepositoryPropertyFields({
  definitions,
  values,
  onChange,
  disabled,
}: {
  definitions: PropertyDefinition[];
  values: PropertyOverrides;
  onChange: (values: PropertyOverrides) => void;
  disabled?: boolean;
}) {
  if (definitions.length === 0) return null;
  return (
    <FieldSet className="min-w-0" disabled={disabled}>
      <FieldLegend variant="label">Repository properties</FieldLegend>
      {definitions.map((definition) => (
        <Field key={definition.id} className="min-w-0 wrap-anywhere">
          <PropertyInput
            definition={definition}
            label={definition.name}
            required={definition.require_explicit_values}
            value={values[definition.id] ?? null}
            unset={
              definition.default === null
                ? "No value"
                : `Inherit default (${display(definition.default)})`
            }
            onChange={(value) => {
              const next = { ...values };
              if (value === null) delete next[definition.id];
              else next[definition.id] = value;
              onChange(next);
            }}
          />
          {definition.description && (
            <FieldDescription>{definition.description}</FieldDescription>
          )}
        </Field>
      ))}
    </FieldSet>
  );
}

export function PropertyInput({
  label,
  definition,
  value,
  unset,
  onChange,
  required = false,
}: {
  label: string;
  definition: PropertyDefinition;
  value: PropertyValue | null;
  unset: string;
  onChange: (value: PropertyValue | null) => void;
  required?: boolean;
}) {
  const caption = required ? `${label} (required)` : label;
  const placeholder = required ? "Choose a value" : unset;
  if (definition.kind === "multi_select") {
    const selected = Array.isArray(value) ? value : [];
    return (
      <fieldset className="grid gap-1 rounded-md border p-3">
        <legend className="font-medium">{caption}</legend>
        {definition.options.map((option) => (
          <label className="flex items-center gap-2" key={option}>
            <Checkbox
              checked={selected.includes(option)}
              onCheckedChange={(checked) => {
                const next =
                  checked === true
                    ? [...selected, option]
                    : selected.filter((item) => item !== option);
                onChange(next.length ? next : null);
              }}
            />
            {option}
          </label>
        ))}
        {!selected.length && (
          <span className="text-sm text-muted-foreground">{placeholder}</span>
        )}
      </fieldset>
    );
  }
  if (definition.kind === "text") {
    return (
      <label className="grid gap-1">
        {caption}
        <Input
          value={typeof value === "string" ? value : ""}
          aria-required={required || undefined}
          placeholder={placeholder}
          maxLength={75}
          onChange={(event) => onChange(event.target.value || null)}
        />
      </label>
    );
  }
  return (
    <label className="grid gap-1">
      {caption}
      <FieldSelect
        className="w-full"
        value={value === null ? "" : display(value)}
        aria-required={required || undefined}
        onValueChange={(nextValue) => {
          const next = nextValue;
          onChange(
            next === ""
              ? null
              : definition.kind === "boolean"
                ? next === "true"
                : next,
          );
        }}
        options={[
          { value: "", label: placeholder },
          ...(definition.kind === "boolean"
            ? ["true", "false"]
            : definition.options
          ).map((option) => ({ value: option, label: option })),
        ]}
      />
    </label>
  );
}
