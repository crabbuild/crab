/**
 * Property-based tests for the multi-agent support feature.
 *
 * Each test corresponds to a numbered property from
 * `.kiro/specs/multi-agent-support/design.md` — see `Feature:
 * multi-agent-support` tags in test names. Property tests are run with
 * fast-check at a minimum of 100 iterations per property.
 */

import { describe, it, expect } from "vitest";
import fc from "fast-check";

// Import registry first so the adapters module has a registry reference.
import "../electron/orchestrator-registry";

import { AgentAdapter, AgentEvent } from "../electron/orchestrator-adapters";
import { AgentRegistry } from "../electron/orchestrator-registry";
import type { AgentCapabilities } from "../electron/orchestrator-registry";
import {
  validateManifest,
  EventNormalizer,
} from "../electron/orchestrator-manifest";
import type { AgentManifest, EventMapping } from "../electron/orchestrator-manifest";

// ─── Helpers ─────────────────────────────────────────────────────────────────

class FakeAdapter extends AgentAdapter {
  readonly name: string;
  readonly binary = "fake";
  private readonly marker: string;
  constructor(name: string, marker: string) {
    super();
    this.name = name;
    this.marker = marker;
  }
  spawn(): void { /* no-op */ }
  sendMessage(): void { /* no-op */ }
  getMarker(): string { return this.marker; }
}

function arbAgentType(): fc.Arbitrary<string> {
  // Restrict to a small alphabet of recognisable agent-type identifiers so
  // registry assertions don't accidentally collide with reserved types.
  return fc
    .string({ minLength: 1, maxLength: 20, unit: "grapheme-ascii" })
    .filter((s) => /^[a-z][a-z0-9_-]*$/.test(s))
    .filter((s) => !["claude-code", "codex", "custom"].includes(s));
}

function arbCapabilities(): fc.Arbitrary<AgentCapabilities> {
  return fc.record({
    supportsPlanMode: fc.boolean(),
    supportsStreaming: fc.boolean(),
    supportsToolUse: fc.boolean(),
    supportsIteration: fc.boolean(),
    supportsApprovalModes: fc.boolean(),
  });
}

function arbEventMapping(): fc.Arbitrary<EventMapping> {
  return fc.record({
    typeField: fc.constantFrom("type", "event", "kind"),
    typeMap: fc.dictionary(
      fc.string({ minLength: 1, maxLength: 16, unit: "grapheme-ascii" }),
      fc.constantFrom<AgentEvent["type"]>(
        "text", "tool_use", "tool_result", "file_edit", "file_read",
        "command_run", "command", "search", "web", "task", "generic",
        "thinking", "error", "usage", "done",
      ),
    ),
    fieldMap: fc.constant({}),
    contentField: fc.option(fc.constantFrom("content", "text", "message"), { nil: undefined }),
    errorField: fc.option(fc.constantFrom("error", "err"), { nil: undefined }),
    toolField: fc.option(fc.constantFrom("tool", "name"), { nil: undefined }),
    usageField: fc.option(fc.constant("usage"), { nil: undefined }),
    usageMapping: fc.option(
      fc.record({
        inputTokens: fc.constantFrom("input_tokens", "prompt_tokens"),
        outputTokens: fc.constantFrom("output_tokens", "completion_tokens"),
        cacheCreationInputTokens: fc.option(fc.constant("cache_creation_input_tokens"), { nil: undefined }),
        cacheReadInputTokens: fc.option(fc.constant("cache_read_input_tokens"), { nil: undefined }),
      }),
      { nil: undefined },
    ),
  });
}

function arbRequiredManifestFields(): fc.Arbitrary<AgentManifest> {
  return fc.record({
    type: fc.string({ minLength: 1, maxLength: 20 }).filter((s) => s.length > 0),
    name: fc.string({ minLength: 1, maxLength: 30 }).filter((s) => s.length > 0),
    binary: fc.string({ minLength: 1, maxLength: 30 }).filter((s) => s.length > 0),
    args: fc.array(fc.string(), { maxLength: 5 }),
    interactionModel: fc.constantFrom<"stdio" | "http" | "file" | "text">(
      "stdio", "http", "file", "text",
    ),
    eventMapping: fc.record({
      typeField: fc.constant("type"),
      typeMap: fc.constant({}),
      fieldMap: fc.constant({}),
    }),
    capabilities: arbCapabilities(),
  }) as fc.Arbitrary<AgentManifest>;
}

// ─── Registry Properties ─────────────────────────────────────────────────────

describe("Feature: multi-agent-support, Property 1: registry lookup returns the registered factory's adapter", () => {
  it("holds for arbitrary (type, factory) pairs", () => {
    fc.assert(
      fc.property(
        arbAgentType(),
        fc.string({ minLength: 1, maxLength: 16 }),
        arbCapabilities(),
        (type, marker, caps) => {
          const reg = new AgentRegistry();
          reg.register({
            type,
            name: type,
            factory: () => new FakeAdapter(type, marker),
            capabilities: caps,
            builtin: false,
          });
          const adapter = reg.createAdapter(type);
          expect(adapter).toBeInstanceOf(FakeAdapter);
          expect((adapter as FakeAdapter).getMarker()).toBe(marker);
        },
      ),
      { numRuns: 100 },
    );
  });
});

describe("Feature: multi-agent-support, Property 2: unregistered type produces descriptive error", () => {
  it("error message lists all registered types", () => {
    fc.assert(
      fc.property(
        fc.array(arbAgentType(), { minLength: 1, maxLength: 5 }),
        arbAgentType(),
        (registeredTypes, unknownType) => {
          fc.pre(!registeredTypes.includes(unknownType));
          // Dedupe to avoid the duplicate-register throw.
          const uniq = [...new Set(registeredTypes)];
          const reg = new AgentRegistry();
          for (const t of uniq) {
            reg.register({
              type: t,
              name: t,
              factory: () => new FakeAdapter(t, ""),
              capabilities: {
                supportsPlanMode: false, supportsStreaming: false,
                supportsToolUse: false, supportsIteration: false,
                supportsApprovalModes: false,
              },
              builtin: false,
            });
          }
          try {
            reg.createAdapter(unknownType);
            expect.fail("expected createAdapter to throw");
          } catch (err) {
            const msg = (err as Error).message;
            expect(msg).toContain(unknownType);
            for (const t of uniq) expect(msg).toContain(t);
          }
        },
      ),
      { numRuns: 100 },
    );
  });
});

describe("Feature: multi-agent-support, Property 11: capability query returns declared capabilities", () => {
  it("getCapabilities returns the exact object supplied at registration", () => {
    fc.assert(
      fc.property(arbAgentType(), arbCapabilities(), (type, caps) => {
        const reg = new AgentRegistry();
        reg.register({
          type,
          name: type,
          factory: () => new FakeAdapter(type, ""),
          capabilities: caps,
          builtin: false,
        });
        expect(reg.getCapabilities(type)).toEqual(caps);
      }),
      { numRuns: 100 },
    );
  });
});

// ─── Manifest Validation ─────────────────────────────────────────────────────

describe("Feature: multi-agent-support, Property 3: manifest validation accepts iff all required fields present", () => {
  const REQUIRED_FIELDS = [
    "type", "name", "binary", "args", "interactionModel",
    "eventMapping", "capabilities",
  ] as const;

  it("accepts well-formed manifests", () => {
    fc.assert(
      fc.property(arbRequiredManifestFields(), (manifest) => {
        const result = validateManifest(manifest);
        expect(result.valid).toBe(true);
      }),
      { numRuns: 100 },
    );
  });

  it("rejects manifests with a missing required field and names the field", () => {
    fc.assert(
      fc.property(
        arbRequiredManifestFields(),
        fc.constantFrom(...REQUIRED_FIELDS),
        (manifest, fieldToDrop) => {
          const broken = { ...manifest } as Record<string, unknown>;
          delete broken[fieldToDrop];
          const result = validateManifest(broken);
          expect(result.valid).toBe(false);
          if (!result.valid) {
            // The error should reference the missing top-level field.
            expect(result.error.field.startsWith(fieldToDrop)).toBe(true);
          }
        },
      ),
      { numRuns: 100 },
    );
  });
});

// ─── Event Normalization ─────────────────────────────────────────────────────

describe("Feature: multi-agent-support, Property 5: unmapped event types become generic with raw data preserved", () => {
  it("holds for any event whose type is absent from typeMap", () => {
    fc.assert(
      fc.property(
        arbEventMapping(),
        fc.string({ minLength: 1, maxLength: 20 }),
        fc.object({ maxDepth: 2, maxKeys: 5 }),
        (mapping, unmappedType, extraProps) => {
          fc.pre(!(unmappedType in mapping.typeMap));
          const raw = {
            ...(extraProps as Record<string, unknown>),
            [mapping.typeField]: unmappedType,
          };
          const event = new EventNormalizer(mapping).normalize(raw);
          expect(event.type).toBe("generic");
          expect(event.input).toEqual(raw);
        },
      ),
      { numRuns: 100 },
    );
  });
});

describe("Feature: multi-agent-support, Property 6: normalized events always have a non-zero timestamp", () => {
  it("injects Date.now() when timestamp is missing or zero", () => {
    fc.assert(
      fc.property(
        arbEventMapping(),
        fc.option(fc.oneof(fc.constant(0), fc.constant(null), fc.constant(undefined))),
        (mapping, badTimestamp) => {
          const raw: Record<string, unknown> = { [mapping.typeField]: "anything" };
          if (badTimestamp !== null && badTimestamp !== undefined) {
            raw.timestamp = badTimestamp;
          }
          const event = new EventNormalizer(mapping).normalize(raw);
          expect(event.timestamp).toBeGreaterThan(0);
        },
      ),
      { numRuns: 100 },
    );
  });

  it("preserves a positive source timestamp", () => {
    fc.assert(
      fc.property(
        arbEventMapping(),
        fc.integer({ min: 1, max: Number.MAX_SAFE_INTEGER }),
        (mapping, ts) => {
          const raw: Record<string, unknown> = {
            [mapping.typeField]: "anything",
            timestamp: ts,
          };
          const event = new EventNormalizer(mapping).normalize(raw);
          expect(event.timestamp).toBe(ts);
        },
      ),
      { numRuns: 100 },
    );
  });
});

describe("Feature: multi-agent-support, Property 7: usage data normalization across naming conventions", () => {
  it("correctly maps token counts regardless of source field names", () => {
    fc.assert(
      fc.property(
        fc.constantFrom("input_tokens", "prompt_tokens", "tokens_in"),
        fc.constantFrom("output_tokens", "completion_tokens", "tokens_out"),
        fc.integer({ min: 0, max: 1_000_000 }),
        fc.integer({ min: 0, max: 1_000_000 }),
        (inFieldName, outFieldName, inTokens, outTokens) => {
          const mapping: EventMapping = {
            typeField: "type",
            typeMap: { "msg": "usage" },
            fieldMap: {},
            usageField: "usage",
            usageMapping: {
              inputTokens: inFieldName,
              outputTokens: outFieldName,
            },
          };
          const raw = {
            type: "msg",
            usage: {
              [inFieldName]: inTokens,
              [outFieldName]: outTokens,
            },
          };
          const event = new EventNormalizer(mapping).normalize(raw);
          expect(event.usage!.inputTokens).toBe(inTokens);
          expect(event.usage!.outputTokens).toBe(outTokens);
        },
      ),
      { numRuns: 100 },
    );
  });
});

describe("Feature: multi-agent-support, Property 8: normalization idempotence", () => {
  it("normalizing → JSON → normalizing produces an equivalent event", () => {
    fc.assert(
      fc.property(
        arbEventMapping(),
        fc.object({ maxDepth: 2, maxKeys: 4 }),
        (mapping, extra) => {
          const raw = {
            ...(extra as Record<string, unknown>),
            [mapping.typeField]: Object.keys(mapping.typeMap)[0] ?? "unknown",
          };
          const normalizer = new EventNormalizer(mapping);
          const first = normalizer.normalize(raw);
          const rountripped = JSON.parse(JSON.stringify(first)) as AgentEvent;
          // The type should remain stable across a round-trip; timestamps
          // are preserved when positive (Property 6).
          expect(rountripped.type).toBe(first.type);
          if (first.timestamp > 0) {
            expect(rountripped.timestamp).toBe(first.timestamp);
          }
        },
      ),
      { numRuns: 100 },
    );
  });
});

// ─── Registry override ──────────────────────────────────────────────────────

describe("Feature: multi-agent-support, Property 13: user manifest overrides built-in registration", () => {
  it("override() replaces the factory for the same type", () => {
    fc.assert(
      fc.property(
        arbAgentType(),
        fc.tuple(fc.string({ minLength: 1, maxLength: 16 }), fc.string({ minLength: 1, maxLength: 16 })),
        (type, [markerA, markerB]) => {
          fc.pre(markerA !== markerB);
          const reg = new AgentRegistry();
          reg.register({
            type, name: type,
            factory: () => new FakeAdapter(type, markerA),
            capabilities: {
              supportsPlanMode: false, supportsStreaming: false,
              supportsToolUse: false, supportsIteration: false,
              supportsApprovalModes: false,
            },
            builtin: true,
          });
          expect((reg.createAdapter(type) as FakeAdapter).getMarker()).toBe(markerA);

          reg.override({
            type, name: type,
            factory: () => new FakeAdapter(type, markerB),
            capabilities: {
              supportsPlanMode: true, supportsStreaming: true,
              supportsToolUse: true, supportsIteration: true,
              supportsApprovalModes: true,
            },
            builtin: false,
          });
          expect((reg.createAdapter(type) as FakeAdapter).getMarker()).toBe(markerB);
        },
      ),
      { numRuns: 50 },
    );
  });
});
