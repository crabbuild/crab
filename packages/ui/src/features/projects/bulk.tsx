import { useId, useState } from "react";
import { Button } from "@/components/ui/button";
import { DatePicker } from "@/components/ui/date-picker";
import { FieldSelect } from "@/components/ui/field-select";
import { Input } from "@/components/ui/input";
import { useProjectCommands } from "./commands";
import {
  priorities,
  type BulkField,
  type Item,
  type ItemPage,
  type Project,
  type ProjectTarget,
  type Row,
} from "./model";

export function useProjectSelection(page: ItemPage) {
  const [selected, setSelected] = useState<Map<string, number>>(new Map());
  const rows = page.items.filter((row) => !row.restricted);
  const chosen = rows.filter((row) => selected.has(row.item.id));
  function toggleGroup(group: Row[], checked: boolean) {
    setSelected((previous) => {
      const next = new Map(previous);
      for (const row of group) {
        if (row.restricted) continue;
        if (checked) next.set(row.item.id, row.version);
        else next.delete(row.item.id);
      }
      return next;
    });
  }
  function groupState(group: Row[]) {
    const eligible = group.filter((row) => !row.restricted);
    const count = eligible.filter((row) => selected.has(row.item.id)).length;
    return {
      empty: !eligible.length,
      checked:
        count === 0
          ? false
          : count === eligible.length
            ? true
            : ("indeterminate" as const),
    };
  }
  return {
    chosen,
    stale: chosen.some((row) => selected.get(row.item.id) !== row.version),
    checked: (row: Row) => selected.has(row.item.id),
    all: groupState(rows).checked,
    empty: rows.length === 0,
    clear: () => setSelected(new Map()),
    toggleGroup,
    groupState,
    toggleAll: (checked: boolean) =>
      setSelected(
        new Map(checked ? rows.map((row) => [row.item.id, row.version]) : []),
      ),
    toggle: (row: Row, checked: boolean) => toggleGroup([row], checked),
  };
}

function original(item: Item, change: BulkField): BulkField {
  if (change.field === "custom")
    return { ...change, value: item.values[change.id] ?? null };
  if (change.field === "status") return { ...change, value: item.status };
  if (change.field === "priority") return { ...change, value: item.priority };
  return { ...change, value: item[change.field] };
}

export function useProjectBulkChange(
  target: ProjectTarget | undefined,
  project: string,
  clear: () => void,
) {
  const commands = useProjectCommands();
  return async (rows: readonly Row[], change: BulkField) => {
    if (!target) return;
    const inverse = rows.flatMap((row) => {
      const previous = original(row.item, change);
      if (previous.value === change.value) return [];
      return [
        { item: row.item.id, version: row.version + 1, change: previous },
      ];
    });
    if (!inverse.length) {
      clear();
      return;
    }
    const result = await commands.submitBulk(
      target,
      {
        kind: "bulk_fields",
        project,
        items: rows.map((row) => ({
          item: row.item.id,
          version: row.version,
          change,
        })),
      },
      inverse,
    );
    if (result?.outcome === "applied") clear();
  };
}

export function ProjectBulkEditor({
  target,
  project,
  selection,
  disabled,
}: {
  target: ProjectTarget;
  project: Project;
  selection: ReturnType<typeof useProjectSelection>;
  disabled: boolean;
}) {
  const dateId = useId();
  const [field, setField] = useState("status");
  const [value, setValue] = useState("");
  const { chosen, stale, clear } = selection;
  const apply = useProjectBulkChange(target, project.id, clear);
  const fields = project.definition.fields;
  const custom = fields.find(({ id }) => field === `field:${id}`);
  const date =
    field === "start" || field === "end" || custom?.definition.kind === "date";
  const options =
    field === "status"
      ? [
          { value: "", label: "Choose a status" },
          ...project.definition.statuses.map((status) => ({
            value: status,
            label: status,
          })),
        ]
      : field === "priority"
        ? priorities.map((label, index) => ({ value: String(index), label }))
        : custom?.definition.kind === "select"
          ? [
              { value: "clear", label: "Clear field" },
              ...custom.definition.options.map((label) => ({
                value: `option:${label}`,
                label,
              })),
            ]
          : undefined;
  let change: BulkField | undefined;
  if (field === "status" && project.definition.statuses.includes(value))
    change = { field, value };
  if (field === "priority") change = { field, value: Number(value) };
  if (field === "start" || field === "end")
    change = { field, value: value || null };
  if (custom)
    change = {
      field: "custom",
      id: custom.id,
      value:
        custom.definition.kind === "select"
          ? value.startsWith("option:")
            ? value.slice(7)
            : null
          : value || null,
    };

  if (!chosen.length) return null;
  return (
    <form
      aria-label="Edit selected Project items"
      className="flex flex-wrap items-center gap-2 py-2"
      onSubmit={(event) => {
        event.preventDefault();
        if (change && !disabled && !stale) void apply(chosen, change);
      }}
    >
      <span className="text-sm">{chosen.length} selected</span>
      <FieldSelect
        label="Field to edit"
        value={field}
        disabled={disabled}
        options={[
          { value: "status", label: "Status" },
          { value: "priority", label: "Priority" },
          { value: "start", label: "Start date" },
          { value: "end", label: "Target date" },
          ...fields.map(({ id, name }) => ({
            value: `field:${id}`,
            label: name,
          })),
        ]}
        onValueChange={(next) => {
          setField(next);
          setValue(next === "priority" ? "0" : "");
        }}
      />
      {options ? (
        <FieldSelect
          label="New value"
          value={
            custom?.definition.kind === "select" && !value ? "clear" : value
          }
          disabled={disabled}
          options={options}
          onValueChange={setValue}
        />
      ) : date ? (
        <DatePicker
          id={dateId}
          label="New date"
          value={value || null}
          disabled={disabled}
          onValueChange={(next) => setValue(next ?? "")}
        />
      ) : (
        <Input
          aria-label="New value"
          type={custom?.definition.kind === "number" ? "number" : "text"}
          step={custom?.definition.kind === "number" ? "any" : undefined}
          placeholder="Blank clears the field"
          className="w-auto"
          value={value}
          disabled={disabled}
          onChange={(event) => setValue(event.target.value)}
        />
      )}
      <Button type="submit" size="sm" disabled={disabled || !change || stale}>
        Apply to selected
      </Button>
      <Button
        type="button"
        variant="ghost"
        size="sm"
        disabled={disabled}
        onClick={clear}
      >
        Clear selection
      </Button>
      {stale && <span role="status">Items changed. Select them again.</span>}
    </form>
  );
}
