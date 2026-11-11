import Ajv, { type ValidateFunction } from "ajv";
import addFormats from "ajv-formats";

export type ElicitationValue = string | number | boolean | string[];

export interface ElicitationField {
  name: string;
  title: string;
  description?: string;
  type: "string" | "number" | "integer" | "boolean" | "array";
  required: boolean;
  options?: Array<{ value: string; label: string }>;
  default?: ElicitationValue;
  minimum?: number;
  maximum?: number;
  minLength?: number;
  maxLength?: number;
  minItems?: number;
  maxItems?: number;
  pattern?: string;
  format?: string;
}

export type AgentElicitationInput = {
  kind: "elicitation";
  requestId: string;
  serverName: string;
  message: string;
  resolved?: boolean;
  resolutionReason?: string;
} & ({ mode: "form"; fields: ElicitationField[] } | { mode: "url"; url: string });

export interface ElicitationResponse {
  kind: "elicitation";
  action: "accept" | "decline" | "cancel";
  content?: Record<string, ElicitationValue>;
}

function object(value: unknown): Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value)) throw new Error("MCP form schema must contain objects");
  return value as Record<string, unknown>;
}

function options(schema: Record<string, unknown>): ElicitationField["options"] {
  const titled = schema.oneOf ?? schema.anyOf;
  if (Array.isArray(titled)) return titled.map(value => {
    const option = object(value);
    if (typeof option.const !== "string" || typeof option.title !== "string") throw new Error("MCP choices require string values and titles");
    return { value: option.const, label: option.title };
  });
  if (!Array.isArray(schema.enum)) return undefined;
  const labels = Array.isArray(schema.enumNames) ? schema.enumNames : [];
  return schema.enum.map((value, index) => {
    if (typeof value !== "string") throw new Error("MCP choices must be strings");
    return { value, label: typeof labels[index] === "string" ? labels[index] : value };
  });
}

/** Normalize the standard MCP form/URL modes shared by both provider transports. */
function parseRequest(input: { requestId: string; serverName: string; message: string; mode?: unknown; schema?: unknown; url?: unknown; propertyOrder?: unknown }): AgentElicitationInput {
  const base = { kind: "elicitation" as const, requestId: input.requestId, serverName: input.serverName, message: input.message };
  if (input.mode === "url") {
    if (typeof input.url !== "string") throw new Error("MCP URL request has no URL");
    const url = new URL(input.url);
    if (!["http:", "https:"].includes(url.protocol) || url.username || url.password) throw new Error("MCP URL must be an HTTP or HTTPS address without embedded credentials");
    return { ...base, mode: "url", url: input.url };
  }
  // MCP specifies form mode when mode is omitted. Extension modes require
  // separate capability negotiation and must not be treated as standard forms.
  if (input.mode !== undefined && input.mode !== "form") throw new Error(`Unsupported MCP elicitation mode: ${String(input.mode)}`);
  const schema = object(input.schema);
  if (schema.type !== "object") throw new Error("MCP forms require a flat object schema");
  const properties = object(schema.properties);
  const required = Array.isArray(schema.required) ? schema.required : [];
  let entries = Object.entries(properties);
  if (input.propertyOrder !== undefined) {
    const order = input.propertyOrder;
    if (!Array.isArray(order) || order.length !== entries.length || new Set(order).size !== order.length || order.some(name => typeof name !== "string" || !Object.hasOwn(properties, name))) throw new Error("Invalid MCP field order");
    entries = order.map(name => [name, properties[name]]);
  }
  const fields = entries.map(([name, value]): ElicitationField => {
    const property = object(value);
    const supported = new Set(["type", "title", "description", "default", "minimum", "maximum", "minLength", "maxLength", "pattern", "format", "enum", "enumNames", "oneOf", "items", "minItems", "maxItems"]);
    if (Object.keys(property).some(keyword => !supported.has(keyword))) throw new Error(`Unsupported MCP schema keyword for ${name}`);
    const type = property.type;
    if (type !== "string" && type !== "number" && type !== "integer" && type !== "boolean" && type !== "array") throw new Error(`Unsupported MCP field type: ${String(type)}`);
    const field: ElicitationField = { name, type, required: required.includes(name), title: typeof property.title === "string" ? property.title : name };
    if (typeof property.description === "string") field.description = property.description;
    for (const key of ["minimum", "maximum", "minLength", "maxLength", "minItems", "maxItems"] as const) {
      if (typeof property[key] === "number") field[key] = property[key];
    }
    if (typeof property.pattern === "string") { new RegExp(property.pattern, "u"); field.pattern = property.pattern; }
    if (typeof property.format === "string") field.format = property.format;
    field.options = options(type === "array" ? object(property.items) : property);
    if (type === "array" && !field.options) throw new Error("MCP arrays require a finite set of string choices");
    if (property.default !== undefined) field.default = property.default as ElicitationValue;
    return field;
  });
  if (required.some(name => typeof name !== "string" || !Object.hasOwn(properties, name))) throw new Error("MCP form requires an undefined property");
  return { ...base, mode: "form", fields };
}

export interface PendingElicitation {
  request: AgentElicitationInput;
  respond: (response: ElicitationResponse) => { action: ElicitationResponse["action"]; content?: Record<string, ElicitationValue> };
}

/** Own validation state only while the provider is waiting for this request. */
export function elicitationRequest(input: Parameters<typeof parseRequest>[0]): PendingElicitation {
  const request = parseRequest(input);
  const validators = new Map<string, { validate: ValidateFunction; required: boolean; title: string }>();
  if (request.mode === "form") {
    const ajv = new Ajv({ allErrors: true });
    addFormats(ajv, ["email", "uri", "date", "date-time"]);
    ajv.addKeyword({ keyword: "enumNames", schemaType: "array" });
    const properties = object(object(input.schema).properties);
    for (const field of request.fields) {
      // Compile values separately: schema-property code generation excludes
      // some JavaScript prototype names, but MCP permits arbitrary field names.
      const validate = ajv.compile(object(properties[field.name]));
      if (field.default !== undefined && !validate(field.default)) throw new Error(`Invalid default for ${field.title}`);
      validators.set(field.name, { validate, required: field.required, title: field.title });
    }
  }
  return { request, respond: response => {
    if (!["accept", "decline", "cancel"].includes(response.action)) throw new Error("Invalid MCP response action");
    if (response.action !== "accept") return { action: response.action };
    if (request.mode === "url") return { action: "accept" };
    const content = object(response.content);
    if (Object.keys(content).some(name => !validators.has(name))) throw new Error("MCP response contains an unrequested field");
    for (const [name, { validate, required, title }] of validators) {
      if (!Object.hasOwn(content, name)) { if (required) throw new Error(`Required: ${title}`); continue; }
      if (!validate(content[name])) throw new Error(`${title}: ${validate.errors?.map(error => error.message).join("; ") ?? "Invalid value"}`);
    }
    return { action: "accept", content: content as Record<string, ElicitationValue> };
  } };
}
