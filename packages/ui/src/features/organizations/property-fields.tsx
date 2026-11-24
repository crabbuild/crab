import { Input } from "@/components/ui/input";

export type PropertyValue = boolean | string | string[];
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

export function PropertyInput({
  label,
  definition,
  value,
  unset,
  onChange,
}: {
  label: string;
  definition: PropertyDefinition;
  value: PropertyValue | null;
  unset: string;
  onChange: (value: PropertyValue | null) => void;
}) {
  if (definition.kind === "multi_select") {
    const selected = Array.isArray(value) ? value : [];
    return (
      <fieldset className="grid gap-1 rounded-md border p-3">
        <legend className="font-medium">{label}</legend>
        {definition.options.map((option) => (
          <label className="flex items-center gap-2" key={option}>
            <input
              type="checkbox"
              checked={selected.includes(option)}
              onChange={(event) => {
                const next = event.target.checked
                  ? [...selected, option]
                  : selected.filter((item) => item !== option);
                onChange(next.length ? next : null);
              }}
            />
            {option}
          </label>
        ))}
        {!selected.length && (
          <span className="text-sm text-muted-foreground">{unset}</span>
        )}
      </fieldset>
    );
  }
  if (definition.kind === "text") {
    return (
      <label className="grid gap-1">
        {label}
        <Input
          value={typeof value === "string" ? value : ""}
          placeholder={unset}
          maxLength={75}
          onChange={(event) => onChange(event.target.value || null)}
        />
      </label>
    );
  }
  return (
    <label className="grid gap-1">
      {label}
      <select
        className={control}
        value={value === null ? "" : display(value)}
        onChange={(event) => {
          const next = event.target.value;
          onChange(
            next === ""
              ? null
              : definition.kind === "boolean"
                ? next === "true"
                : next,
          );
        }}
      >
        <option value="">{unset}</option>
        {(definition.kind === "boolean"
          ? ["true", "false"]
          : definition.options
        ).map((option) => (
          <option key={option} value={option}>
            {option}
          </option>
        ))}
      </select>
    </label>
  );
}
