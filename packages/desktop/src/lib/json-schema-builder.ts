export type SchemaBuilderFieldType = "STR" | "NUM" | "BOOL" | "ENUM" | "OBJ" | "ARR";

export interface SchemaBuilderField {
  id: string;
  name: string;
  type: SchemaBuilderFieldType;
  description?: string;
  required?: boolean;
  enumValues?: string[];
  properties?: SchemaBuilderField[];
  arrayItem?: SchemaBuilderField;
}

export interface SchemaBuilderModel {
  name: string;
  properties: SchemaBuilderField[];
}

export interface SchemaBuilderParseResult {
  model: SchemaBuilderModel;
  advancedOnly: boolean;
}

export type JsonSchemaObject = Record<string, unknown>;

export function schemaBuilderModelToJsonSchema(model: SchemaBuilderModel): JsonSchemaObject {
  const required = model.properties
    .filter((field) => field.required && field.name.trim())
    .map((field) => field.name.trim());
  return compactSchema({
    title: model.name.trim() || undefined,
    type: "object",
    properties: Object.fromEntries(model.properties
      .filter((field) => field.name.trim())
      .map((field) => [field.name.trim(), fieldToJsonSchema(field)])),
    required: required.length > 0 ? required : undefined,
    additionalProperties: false,
  });
}

export function jsonSchemaToBuilderModel(schema: JsonSchemaObject | undefined, createId: () => string): SchemaBuilderParseResult {
  if (!schema || Object.keys(schema).length === 0) {
    return {
      advancedOnly: false,
      model: {
        name: "schema",
        properties: [],
      },
    };
  }
  if (schema.type !== "object" || !isPlainObject(schema.properties)) {
    return {
      advancedOnly: true,
      model: {
        name: typeof schema.title === "string" ? schema.title : "schema",
        properties: [],
      },
    };
  }

  const required = new Set(Array.isArray(schema.required) ? schema.required.filter((item): item is string => typeof item === "string") : []);
  const fields: SchemaBuilderField[] = [];
  let advancedOnly = false;
  for (const [name, childSchema] of Object.entries(schema.properties)) {
    const parsed = jsonSchemaPropertyToField(name, childSchema, required.has(name), createId);
    if (!parsed) {
      advancedOnly = true;
      continue;
    }
    if (parsed.advancedOnly) advancedOnly = true;
    fields.push(parsed.field);
  }

  return {
    advancedOnly,
    model: {
      name: typeof schema.title === "string" && schema.title.trim() ? schema.title : "schema",
      properties: fields,
    },
  };
}

export function createSchemaBuilderField(createId: () => string, type: SchemaBuilderFieldType = "STR"): SchemaBuilderField {
  return {
    id: createId(),
    name: "",
    type,
    description: "",
    required: true,
    enumValues: type === "ENUM" ? [""] : undefined,
    properties: type === "OBJ" ? [] : undefined,
    arrayItem: type === "ARR" ? createSchemaBuilderField(createId, "STR") : undefined,
  };
}

function fieldToJsonSchema(field: SchemaBuilderField): JsonSchemaObject {
  const base = compactSchema({
    description: field.description?.trim() || undefined,
  });
  switch (field.type) {
    case "STR":
      return { ...base, type: "string" };
    case "NUM":
      return { ...base, type: "number" };
    case "BOOL":
      return { ...base, type: "boolean" };
    case "ENUM":
      {
        const enumValues = (field.enumValues ?? []).map((item) => item.trim()).filter(Boolean);
        return compactSchema({
          ...base,
          type: "string",
          enum: enumValues.length > 0 ? enumValues : undefined,
        });
      }
    case "OBJ": {
      const properties = field.properties ?? [];
      const required = properties
        .filter((child) => child.required && child.name.trim())
        .map((child) => child.name.trim());
      return compactSchema({
        ...base,
        type: "object",
        properties: Object.fromEntries(properties
          .filter((child) => child.name.trim())
          .map((child) => [child.name.trim(), fieldToJsonSchema(child)])),
        required: required.length > 0 ? required : undefined,
        additionalProperties: false,
      });
    }
    case "ARR":
      return compactSchema({
        ...base,
        type: "array",
        items: field.arrayItem ? fieldToJsonSchema(field.arrayItem) : { type: "string" },
      });
  }
}

function jsonSchemaPropertyToField(
  name: string,
  schema: unknown,
  required: boolean,
  createId: () => string,
): { field: SchemaBuilderField; advancedOnly: boolean } | null {
  if (!isPlainObject(schema)) return null;
  const description = typeof schema.description === "string" ? schema.description : "";
  const common = {
    id: createId(),
    name,
    description,
    required,
  };

  if (Array.isArray(schema.enum) && schema.enum.every((item) => typeof item === "string")) {
    return {
      advancedOnly: schema.type !== "string",
      field: {
        ...common,
        type: "ENUM",
        enumValues: schema.enum,
      },
    };
  }

  if (schema.type === "string") return { advancedOnly: false, field: { ...common, type: "STR" } };
  if (schema.type === "number" || schema.type === "integer") return { advancedOnly: schema.type === "integer", field: { ...common, type: "NUM" } };
  if (schema.type === "boolean") return { advancedOnly: false, field: { ...common, type: "BOOL" } };
  if (schema.type === "object" && isPlainObject(schema.properties)) {
    const childRequired = new Set(Array.isArray(schema.required) ? schema.required.filter((item): item is string => typeof item === "string") : []);
    const children: SchemaBuilderField[] = [];
    let advancedOnly = schema.additionalProperties !== false && schema.additionalProperties !== undefined;
    for (const [childName, childSchema] of Object.entries(schema.properties)) {
      const parsed = jsonSchemaPropertyToField(childName, childSchema, childRequired.has(childName), createId);
      if (!parsed) {
        advancedOnly = true;
        continue;
      }
      if (parsed.advancedOnly) advancedOnly = true;
      children.push(parsed.field);
    }
    return {
      advancedOnly,
      field: {
        ...common,
        type: "OBJ",
        properties: children,
      },
    };
  }
  if (schema.type === "array") {
    const parsedItem = jsonSchemaPropertyToField("item", schema.items, true, createId);
    return {
      advancedOnly: !parsedItem || parsedItem.advancedOnly,
      field: {
        ...common,
        type: "ARR",
        arrayItem: parsedItem?.field ?? createSchemaBuilderField(createId, "STR"),
      },
    };
  }

  return null;
}

function compactSchema(schema: JsonSchemaObject): JsonSchemaObject {
  return Object.fromEntries(Object.entries(schema).filter(([, value]) => value !== undefined));
}

function isPlainObject(value: unknown): value is Record<string, unknown> {
  return Boolean(value) && typeof value === "object" && !Array.isArray(value);
}
