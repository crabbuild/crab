import { useEffect, useMemo, useState } from "react";
import clsx from "clsx";
import { Check, Plus, Trash2, Wand2, X } from "lucide-react";

import {
  createSchemaBuilderField,
  jsonSchemaToBuilderModel,
  schemaBuilderModelToJsonSchema,
} from "../../lib/json-schema-builder";
import type {
  SchemaBuilderField,
  SchemaBuilderFieldType,
  SchemaBuilderModel,
} from "../../lib/json-schema-builder";

type JsonSchemaObject = Record<string, unknown>;

interface JsonSchemaBuilderDialogProps {
  open: boolean;
  schema: JsonSchemaObject | undefined;
  schemaName: string | undefined;
  onApply: (schema: JsonSchemaObject | undefined, schemaName: string | undefined) => void;
  onClose: () => void;
}

const FIELD_TYPES: Array<{ value: SchemaBuilderFieldType; label: string }> = [
  { value: "STR", label: "String" },
  { value: "NUM", label: "Number" },
  { value: "BOOL", label: "Boolean" },
  { value: "ENUM", label: "Enum" },
  { value: "OBJ", label: "Object" },
  { value: "ARR", label: "Array" },
];

const MODE_LABELS: Record<"simple" | "advanced", string> = {
  simple: "Builder",
  advanced: "JSON",
};

export function JsonSchemaBuilderDialog({
  open,
  schema,
  schemaName,
  onApply,
  onClose,
}: JsonSchemaBuilderDialogProps) {
  const [mode, setMode] = useState<"simple" | "advanced">("simple");
  const [model, setModel] = useState<SchemaBuilderModel>(() => ({ name: schemaName || "schema", properties: [] }));
  const [advancedDraft, setAdvancedDraft] = useState("{}");
  const [advancedError, setAdvancedError] = useState<string | null>(null);
  const [advancedOnly, setAdvancedOnly] = useState(false);

  useEffect(() => {
    if (!open) return;
    const parsed = jsonSchemaToBuilderModel(schema, createLocalId);
    setModel({
      ...parsed.model,
      name: schemaName || parsed.model.name || "schema",
    });
    setAdvancedDraft(JSON.stringify(schema ?? {}, null, 2));
    setAdvancedError(null);
    setAdvancedOnly(parsed.advancedOnly);
    setMode(parsed.advancedOnly ? "advanced" : "simple");
  }, [open, schema, schemaName]);

  const simpleSchema = useMemo(() => schemaBuilderModelToJsonSchema(model), [model]);

  if (!open) return null;

  const commitAdvanced = () => {
    try {
      const parsed = advancedDraft.trim() ? JSON.parse(advancedDraft) as unknown : {};
      if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
        throw new Error("Schema must be a JSON object.");
      }
      const schemaObject = parsed as JsonSchemaObject;
      const nextName = typeof schemaObject.title === "string" && schemaObject.title.trim()
        ? schemaObject.title.trim()
        : model.name.trim() || undefined;
      onApply(Object.keys(schemaObject).length > 0 ? schemaObject : undefined, nextName);
      onClose();
    } catch (err) {
      setAdvancedError(err instanceof Error ? err.message : "Invalid JSON.");
    }
  };

  const commitSimple = () => {
    onApply(model.properties.some((field) => field.name.trim()) ? simpleSchema : undefined, model.name.trim() || undefined);
    onClose();
  };

  return (
    <div className="fixed inset-0 z-[80] flex items-center justify-center bg-black/50 p-4 backdrop-blur-sm" role="dialog" aria-modal="true" aria-labelledby="schema-builder-title">
      <div className="flex max-h-[86vh] w-[760px] max-w-full flex-col overflow-hidden rounded-lg border border-gx-border bg-gx-surface shadow-2xl">
        <div className="flex items-center gap-3 border-b border-gx-border px-4 py-3">
          <div className="min-w-0 flex-1">
            <h2 id="schema-builder-title" className="text-sm font-semibold text-gx-text">Output Schema</h2>
            <p className="mt-0.5 text-[11px] text-gx-muted">Define the JSON fields downstream branches and mappings can rely on.</p>
          </div>
          <div className="flex overflow-hidden rounded-md border border-gx-border bg-gx-bg-sub p-0.5">
            {(["simple", "advanced"] as const).map((item) => (
              <button
                key={item}
                type="button"
                className={clsx(
                  "rounded px-3 py-1 text-xs font-medium transition",
                  mode === item ? "bg-gx-surface-hover text-gx-text shadow-sm" : "text-gx-muted hover:text-gx-text",
                )}
                onClick={() => setMode(item)}
              >
                {MODE_LABELS[item]}
              </button>
            ))}
          </div>
          <button type="button" className="btn-icon h-7 w-7" onClick={onClose} aria-label="Close schema builder">
            <X size={14} />
          </button>
        </div>

        <div className="min-h-0 flex-1 overflow-auto px-4 py-3">
          {mode === "simple" ? (
            <div className="space-y-4">
              {advancedOnly && (
                <div className="rounded border border-gx-warn/40 bg-gx-warn/10 px-3 py-2 text-xs text-gx-warn">
                  This schema contains advanced JSON Schema fields. Simple edits will keep only supported fields.
                </div>
              )}
              <label className="block space-y-1 text-xs text-gx-muted">
                <span>Schema name</span>
                <input
                  className="input-field h-9 w-full text-sm"
                  value={model.name}
                  onChange={(event) => setModel({ ...model, name: event.target.value })}
                  placeholder="schema"
                />
              </label>
              <div className="space-y-2">
                <div className="grid grid-cols-[28px_1fr_86px_1.2fr_74px_32px] items-center gap-2 border-b border-gx-border pb-2 text-[11px] text-gx-muted">
                  <span />
                  <span>Name</span>
                  <span>Type</span>
                  <span>Description</span>
                  <span>Required</span>
                  <span />
                </div>
                {model.properties.length === 0 ? (
                  <div className="rounded border border-dashed border-gx-border bg-gx-bg-sub/40 p-4 text-center text-xs text-gx-muted">
                    Add fields such as <span className="font-mono text-gx-text">result</span>, <span className="font-mono text-gx-text">score</span>, or <span className="font-mono text-gx-text">status</span>.
                  </div>
                ) : (
                  <FieldList
                    fields={model.properties}
                    onChange={(properties) => setModel({ ...model, properties })}
                  />
                )}
              </div>
              <button
                type="button"
                className="btn flex h-8 items-center gap-1.5 text-xs"
                onClick={() => setModel({ ...model, properties: [...model.properties, createSchemaBuilderField(createLocalId)] })}
              >
                <Plus size={13} />
                Add property
              </button>
            </div>
          ) : (
            <div className="space-y-2">
              <textarea
                className={clsx("input-field min-h-[360px] w-full resize-y font-mono text-[11px]", advancedError && "border-gx-error")}
                value={advancedDraft}
                spellCheck={false}
                onChange={(event) => {
                  setAdvancedDraft(event.target.value);
                  setAdvancedError(null);
                }}
              />
              {advancedError && <div className="text-xs text-gx-error">{advancedError}</div>}
            </div>
          )}
        </div>

        <div className="flex items-center gap-2 border-t border-gx-border px-4 py-3">
          <button
            type="button"
            className="btn flex items-center gap-1.5 text-xs"
            title="Generate schema draft"
            onClick={() => {
              if (mode === "simple" && model.properties.length === 0) {
                setModel({
                  ...model,
                  properties: [{ ...createSchemaBuilderField(createLocalId, "STR"), name: "result", description: "result" }],
                });
              }
            }}
          >
            <Wand2 size={13} />
            Starter field
          </button>
          <div className="flex-1" />
          <button type="button" className="btn text-xs" onClick={onClose}>Cancel</button>
          <button
            type="button"
            className="btn btn-primary flex items-center gap-1.5 text-xs"
            onClick={mode === "simple" ? commitSimple : commitAdvanced}
          >
            <Check size={13} />
            Update
          </button>
        </div>
      </div>
    </div>
  );
}

function FieldList({
  fields,
  onChange,
}: {
  fields: SchemaBuilderField[];
  onChange: (fields: SchemaBuilderField[]) => void;
}) {
  const updateField = (id: string, field: SchemaBuilderField) => {
    onChange(fields.map((item) => item.id === id ? field : item));
  };
  return (
    <div className="space-y-2">
      {fields.map((field) => (
        <FieldEditor
          key={field.id}
          field={field}
          onChange={(next) => updateField(field.id, next)}
          onDelete={() => onChange(fields.filter((item) => item.id !== field.id))}
        />
      ))}
    </div>
  );
}

function FieldEditor({
  field,
  onChange,
  onDelete,
}: {
  field: SchemaBuilderField;
  onChange: (field: SchemaBuilderField) => void;
  onDelete: () => void;
}) {
  const updateType = (type: SchemaBuilderFieldType) => {
    onChange({
      ...field,
      type,
      enumValues: type === "ENUM" ? field.enumValues ?? [""] : undefined,
      properties: type === "OBJ" ? field.properties ?? [] : undefined,
      arrayItem: type === "ARR" ? field.arrayItem ?? createSchemaBuilderField(createLocalId, "STR") : undefined,
    });
  };

  return (
    <div className="space-y-2 rounded border border-gx-border bg-gx-bg-sub/50 p-2">
      <div className="grid grid-cols-[28px_1fr_86px_1.2fr_74px_32px] items-center gap-2">
        <input
          type="checkbox"
          className="justify-self-center"
          checked={field.required ?? false}
          onChange={(event) => onChange({ ...field, required: event.target.checked })}
          aria-label={`Require ${field.name || "property"}`}
        />
        <input
          className="input-field h-8 min-w-0 text-xs"
          value={field.name}
          onChange={(event) => onChange({ ...field, name: event.target.value })}
          placeholder="result"
        />
        <select
          className="input-field h-8 min-w-0 text-xs"
          value={field.type}
          onChange={(event) => updateType(event.target.value as SchemaBuilderFieldType)}
        >
          {FIELD_TYPES.map((type) => (
            <option key={type.value} value={type.value}>{type.label}</option>
          ))}
        </select>
        <input
          className="input-field h-8 min-w-0 text-xs"
          value={field.description ?? ""}
          onChange={(event) => onChange({ ...field, description: event.target.value })}
          placeholder="result"
        />
        <span className={clsx("text-center text-[10px]", field.required ? "text-gx-accent" : "text-gx-muted")}>
          {field.required ? "Yes" : "No"}
        </span>
        <button type="button" className="btn-icon h-7 w-7 text-gx-error" onClick={onDelete} aria-label={`Delete ${field.name || "property"}`}>
          <Trash2 size={13} />
        </button>
      </div>
      {field.type === "ENUM" && (
        <EnumValueEditor
          values={field.enumValues ?? []}
          onChange={(enumValues) => onChange({ ...field, enumValues })}
        />
      )}
      {field.type === "OBJ" && (
        <NestedFieldEditor
          fields={field.properties ?? []}
          onChange={(properties) => onChange({ ...field, properties })}
        />
      )}
      {field.type === "ARR" && (
        <ArrayItemEditor
          item={field.arrayItem ?? createSchemaBuilderField(createLocalId, "STR")}
          onChange={(arrayItem) => onChange({ ...field, arrayItem })}
        />
      )}
    </div>
  );
}

function EnumValueEditor({ values, onChange }: { values: string[]; onChange: (values: string[]) => void }) {
  const list = values.length > 0 ? values : [""];
  return (
    <div className="flex flex-wrap gap-1.5 pl-7">
      {list.map((value, index) => (
        <div key={index} className="flex items-center gap-1">
          <input
            className="input-field h-7 w-28 font-mono text-[11px]"
            value={value}
            onChange={(event) => onChange(list.map((item, itemIndex) => itemIndex === index ? event.target.value : item))}
            placeholder="value"
          />
          {list.length > 1 && (
            <button
              type="button"
              className="btn-icon h-7 w-7 text-gx-error"
              onClick={() => onChange(list.filter((_, itemIndex) => itemIndex !== index))}
              aria-label={`Delete enum value ${index + 1}`}
            >
              <Trash2 size={12} />
            </button>
          )}
        </div>
      ))}
      <button type="button" className="btn h-7 text-[11px]" onClick={() => onChange([...list, ""])}>Add value</button>
    </div>
  );
}

function NestedFieldEditor({
  fields,
  onChange,
}: {
  fields: SchemaBuilderField[];
  onChange: (fields: SchemaBuilderField[]) => void;
}) {
  return (
    <div className="space-y-2 border-l border-gx-border pl-4">
      <FieldList fields={fields} onChange={onChange} />
      <button type="button" className="btn h-7 text-[11px]" onClick={() => onChange([...fields, createSchemaBuilderField(createLocalId)])}>
        Add nested property
      </button>
    </div>
  );
}

function ArrayItemEditor({
  item,
  onChange,
}: {
  item: SchemaBuilderField;
  onChange: (field: SchemaBuilderField) => void;
}) {
  return (
    <div className="space-y-2 border-l border-gx-border pl-4">
      <div className="text-[11px] text-gx-muted">Array item</div>
      <FieldEditor field={{ ...item, name: item.name || "item" }} onChange={onChange} onDelete={() => onChange(createSchemaBuilderField(createLocalId, "STR"))} />
    </div>
  );
}

function createLocalId(): string {
  if (typeof crypto !== "undefined" && "randomUUID" in crypto) {
    return crypto.randomUUID();
  }
  return `${Date.now()}-${Math.random().toString(36).slice(2)}`;
}
