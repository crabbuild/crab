import { describe, expect, it } from "vitest";
import { elicitationRequest } from "../electron/agent-elicitation";

const request = (properties: Record<string, unknown>, required: string[] = []) => elicitationRequest({ requestId: "request-1", serverName: "Fixture server", message: "Provide values", schema: { type: "object", properties, required } });

describe("standard MCP elicitation", () => {
  it("preserves primitive types, titled choices, optional omissions, and empty accepted objects", () => {
    const form = request({ count: { type: "integer", minimum: 0 }, enabled: { type: "boolean", default: false }, name: { type: "string" }, mode: { type: "string", oneOf: [{ const: "thorough", title: "Thorough review" }] }, checks: { type: "array", items: { anyOf: [{ const: "build", title: "Build project" }] } }, optional: { type: "number" } }, ["count", "enabled", "name"]);
    const content = { count: 0, enabled: false, name: "", mode: "thorough", checks: ["build"] };
    expect({ typed: form.respond({ kind: "elicitation", action: "accept", content }), empty: request({}).respond({ kind: "elicitation", action: "accept", content: {} }) }).toEqual({
      typed: { action: "accept", content }, empty: { action: "accept", content: {} },
    });
  });

  it("validates arbitrary property names without treating inherited properties as input", () => {
    const properties = JSON.parse('{"__proto__":{"type":"string"},"constructor":{"type":"string"}}') as Record<string, unknown>;
    const content = JSON.parse('{"__proto__":"value"}') as Record<string, string>;
    const form = request(properties, ["__proto__"]);
    expect(form.respond({ kind: "elicitation", action: "accept", content })).toEqual({ action: "accept", content });
  });

  it("honors the provider's explicit field order", () => {
    const form = elicitationRequest({ requestId: "ordered", serverName: "Fixture", message: "Ordered", schema: { type: "object", properties: { first: { type: "string" }, second: { type: "string" } } }, propertyOrder: ["second", "first"] });
    expect(form.request.mode === "form" ? form.request.fields.map(field => field.name) : []).toEqual(["second", "first"]);
  });

  it("rejects unsupported constraints rather than silently weakening the requested schema", () => {
    expect(() => request({ value: { type: "string", const: "fixed" } })).toThrow("Unsupported MCP schema keyword");
  });

  it("rejects contradictory array item schemas rather than discarding their type", () => {
    expect(() => request({ values: { type: "array", items: { type: "number", enum: ["invalid"] } } }).respond({ kind: "elicitation", action: "accept", content: { values: ["invalid"] } })).toThrow();
  });

  it.each([null, 1, false])("rejects an invalid string default: %s", value => {
    expect(() => request({ text: { type: "string", default: value } })).toThrow("Invalid default");
  });

  it.each([
    ["number type", { count: { type: "integer" } }, { count: "2" }],
    ["integer", { count: { type: "integer" } }, { count: 1.5 }],
    ["minimum", { count: { type: "number", minimum: 2 } }, { count: 1 }],
    ["maximum", { count: { type: "number", maximum: 2 } }, { count: 3 }],
    ["boolean", { enabled: { type: "boolean" } }, { enabled: "false" }],
    ["length", { text: { type: "string", minLength: 2 } }, { text: "🦀" }],
    ["pattern", { text: { type: "string", pattern: "^[A-Z]+$" } }, { text: "lowercase" }],
    ["date", { date: { type: "string", format: "date" } }, { date: "2026-02-30" }],
    ["date-time", { date: { type: "string", format: "date-time" } }, { date: "2026-11-08T12:00:00" }],
    ["enum", { mode: { type: "string", enum: ["fast"] } }, { mode: "other" }],
    ["array membership", { checks: { type: "array", items: { type: "string", enum: ["build"] } } }, { checks: ["other"] }],
    ["array size", { checks: { type: "array", maxItems: 1, items: { type: "string", enum: ["build", "test"] } } }, { checks: ["build", "test"] }],
    ["extra property", {}, { unrequested: "value" }],
  ] as const)("rejects invalid %s before any provider response", (_name, properties, content) => {
    const form = request(properties);
    expect(() => form.respond({ kind: "elicitation", action: "accept", content: { ...content } })).toThrow();
  });

  it("requires a present value rather than truthiness", () => {
    const form = request({ enabled: { type: "boolean" } }, ["enabled"]);
    expect(() => form.respond({ kind: "elicitation", action: "accept", content: {} })).toThrow("Required");
  });

  it.each(["decline", "cancel"] as const)("does not send form contents with %s", action => {
    expect(request({}).respond({ kind: "elicitation", action, content: { unwanted: "value" } })).toEqual({ action });
  });

  it.each(["javascript:alert(1)", "file:///etc/passwd", "data:text/html,fixture", "https://user:password@example.invalid/"])("rejects unsafe URL mode: %s", url => {
    expect(() => elicitationRequest({ requestId: "url", serverName: "Fixture", message: "Open the page", mode: "url", url })).toThrow();
  });

  it("keeps the full URL for consent and returns no form data for URL acceptance", () => {
    const url = "https://auth.example.invalid/connect?flow=fixture";
    const link = elicitationRequest({ requestId: "url", serverName: "Fixture", message: "Open the page", mode: "url", url });
    expect({ request: link.request, response: link.respond({ kind: "elicitation", action: "accept", content: { omitted: "value" } }) }).toEqual({
      request: { kind: "elicitation", requestId: "url", serverName: "Fixture", message: "Open the page", mode: "url", url }, response: { action: "accept" },
    });
  });

  it("rejects extension modes that were not negotiated", () => {
    expect(() => elicitationRequest({ requestId: "extension", serverName: "Fixture", message: "Extension", mode: "openai/form" })).toThrow("Unsupported MCP elicitation mode");
  });
});
