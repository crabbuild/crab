import { describe, expect, it } from "vitest";

import {
  jsonSchemaToBuilderModel,
  schemaBuilderModelToJsonSchema,
} from "../src/lib/json-schema-builder";
import type { SchemaBuilderModel } from "../src/lib/json-schema-builder";

let nextId = 0;
function createId(): string {
  nextId += 1;
  return `field-${nextId}`;
}

describe("JSON schema builder", () => {
  it("emits object schemas with required fields, enums, nested objects, arrays, and closed objects", () => {
    const model: SchemaBuilderModel = {
      name: "schema",
      properties: [
        { id: "result", name: "result", type: "STR", description: "result", required: true },
        { id: "score", name: "score", type: "NUM", required: true },
        { id: "ok", name: "ok", type: "BOOL", required: false },
        { id: "status", name: "status", type: "ENUM", required: true, enumValues: ["open", "closed"] },
        {
          id: "meta",
          name: "meta",
          type: "OBJ",
          required: false,
          properties: [{ id: "region", name: "region", type: "STR", required: true }],
        },
        {
          id: "tags",
          name: "tags",
          type: "ARR",
          required: false,
          arrayItem: { id: "tag", name: "item", type: "STR", required: true },
        },
      ],
    };

    expect(schemaBuilderModelToJsonSchema(model)).toEqual({
      title: "schema",
      type: "object",
      properties: {
        result: { type: "string", description: "result" },
        score: { type: "number" },
        ok: { type: "boolean" },
        status: { type: "string", enum: ["open", "closed"] },
        meta: {
          type: "object",
          properties: { region: { type: "string" } },
          required: ["region"],
          additionalProperties: false,
        },
        tags: { type: "array", items: { type: "string" } },
      },
      required: ["result", "score", "status"],
      additionalProperties: false,
    });
  });

  it("round-trips supported schemas into simple mode", () => {
    const parsed = jsonSchemaToBuilderModel({
      title: "schema",
      type: "object",
      properties: {
        result: { type: "string" },
      },
      required: ["result"],
      additionalProperties: false,
    }, createId);

    expect(parsed.advancedOnly).toBe(false);
    expect(parsed.model.properties).toMatchObject([{ name: "result", type: "STR", required: true }]);
  });

  it("marks unsupported schemas as advanced-only", () => {
    const parsed = jsonSchemaToBuilderModel({
      type: "object",
      properties: {
        value: { type: ["string", "number"] },
      },
    }, createId);

    expect(parsed.advancedOnly).toBe(true);
    expect(parsed.model.properties).toEqual([]);
  });
});
