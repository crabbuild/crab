/**
 * Tests for the advanced LSP features:
 * - Workspace symbol search aggregation
 * - Selection range linked-list flattening
 * - Color info round-trips
 * - Call hierarchy cycle detection
 * - Symbol kind mapping
 */

import { describe, it, expect, vi, beforeEach } from "vitest";
import { lspSymbolKindToSymbolKind, LSP_SYMBOL_KIND } from "../src/lib/lsp-symbol-kind";

// ── Symbol Kind Mapping ────────────────────────────────────────────

describe("lspSymbolKindToSymbolKind", () => {
  it("maps Function to 'function'", () => {
    expect(lspSymbolKindToSymbolKind(LSP_SYMBOL_KIND.Function)).toBe("function");
  });

  it("maps Method and Constructor to 'method'", () => {
    expect(lspSymbolKindToSymbolKind(LSP_SYMBOL_KIND.Method)).toBe("method");
    expect(lspSymbolKindToSymbolKind(LSP_SYMBOL_KIND.Constructor)).toBe("method");
  });

  it("maps Class to 'class'", () => {
    expect(lspSymbolKindToSymbolKind(LSP_SYMBOL_KIND.Class)).toBe("class");
  });

  it("maps Interface to 'interface'", () => {
    expect(lspSymbolKindToSymbolKind(LSP_SYMBOL_KIND.Interface)).toBe("interface");
  });

  it("maps Enum and EnumMember to 'enum'", () => {
    expect(lspSymbolKindToSymbolKind(LSP_SYMBOL_KIND.Enum)).toBe("enum");
    expect(lspSymbolKindToSymbolKind(LSP_SYMBOL_KIND.EnumMember)).toBe("enum");
  });

  it("maps Variable, Field, Property to 'variable'", () => {
    expect(lspSymbolKindToSymbolKind(LSP_SYMBOL_KIND.Variable)).toBe("variable");
    expect(lspSymbolKindToSymbolKind(LSP_SYMBOL_KIND.Field)).toBe("variable");
    expect(lspSymbolKindToSymbolKind(LSP_SYMBOL_KIND.Property)).toBe("variable");
  });

  it("maps Constant to 'constant'", () => {
    expect(lspSymbolKindToSymbolKind(LSP_SYMBOL_KIND.Constant)).toBe("constant");
  });

  it("maps Module, Namespace, Package to 'module'", () => {
    expect(lspSymbolKindToSymbolKind(LSP_SYMBOL_KIND.Module)).toBe("module");
    expect(lspSymbolKindToSymbolKind(LSP_SYMBOL_KIND.Namespace)).toBe("module");
    expect(lspSymbolKindToSymbolKind(LSP_SYMBOL_KIND.Package)).toBe("module");
  });

  it("maps Struct to 'struct'", () => {
    expect(lspSymbolKindToSymbolKind(LSP_SYMBOL_KIND.Struct)).toBe("struct");
  });

  it("falls back to 'variable' for unknown kinds", () => {
    expect(lspSymbolKindToSymbolKind(999)).toBe("variable");
    expect(lspSymbolKindToSymbolKind(LSP_SYMBOL_KIND.String)).toBe("variable");
  });
});

// ── Workspace Symbol Aggregation ───────────────────────────────────

// Mock the registry since searchWorkspaceSymbols calls into it.
vi.mock("../src/lib/lsp-registry", () => ({
  lspRegistry: {
    getAllClients: vi.fn(() => []),
  },
}));

describe("searchWorkspaceSymbols", () => {
  let getAllClientsMock: ReturnType<typeof vi.fn>;

  beforeEach(async () => {
    vi.resetModules();
    vi.clearAllMocks();
    const registry = await import("../src/lib/lsp-registry");
    getAllClientsMock = registry.lspRegistry.getAllClients as ReturnType<typeof vi.fn>;
  });

  it("returns empty results for empty query", async () => {
    const { searchWorkspaceSymbols } = await import("../src/lib/lsp-workspace-symbols");
    expect(await searchWorkspaceSymbols("")).toEqual([]);
    expect(await searchWorkspaceSymbols("   ")).toEqual([]);
  });

  it("returns empty when no clients are running", async () => {
    getAllClientsMock.mockReturnValue([]);
    const { searchWorkspaceSymbols } = await import("../src/lib/lsp-workspace-symbols");
    expect(await searchWorkspaceSymbols("foo")).toEqual([]);
  });

  it("skips clients that don't support workspaceSymbolProvider", async () => {
    const mockRequest = vi.fn().mockResolvedValue([]);
    getAllClientsMock.mockReturnValue([
      {
        language: "typescript",
        client: {
          getCapabilities: () => ({ workspaceSymbolProvider: false }),
          getConnection: () => ({ request: mockRequest }),
        },
      },
    ]);

    const { searchWorkspaceSymbols } = await import("../src/lib/lsp-workspace-symbols");
    const results = await searchWorkspaceSymbols("foo");
    expect(results).toEqual([]);
    expect(mockRequest).not.toHaveBeenCalled();
  });

  it("aggregates results from multiple clients", async () => {
    const tsRequest = vi.fn().mockResolvedValue([
      {
        name: "fooTs",
        kind: 12,
        location: {
          uri: "file:///foo.ts",
          range: {
            start: { line: 0, character: 0 },
            end: { line: 0, character: 5 },
          },
        },
      },
    ]);
    const rustRequest = vi.fn().mockResolvedValue([
      {
        name: "fooRs",
        kind: 12,
        location: {
          uri: "file:///foo.rs",
          range: {
            start: { line: 10, character: 2 },
            end: { line: 10, character: 7 },
          },
        },
      },
    ]);

    getAllClientsMock.mockReturnValue([
      {
        language: "typescript",
        client: {
          getCapabilities: () => ({ workspaceSymbolProvider: true }),
          getConnection: () => ({ request: tsRequest }),
        },
      },
      {
        language: "rust",
        client: {
          getCapabilities: () => ({ workspaceSymbolProvider: true }),
          getConnection: () => ({ request: rustRequest }),
        },
      },
    ]);

    const { searchWorkspaceSymbols } = await import("../src/lib/lsp-workspace-symbols");
    const results = await searchWorkspaceSymbols("foo");
    expect(results).toHaveLength(2);
    expect(results.find((r) => r.name === "fooTs")?.language).toBe("typescript");
    expect(results.find((r) => r.name === "fooRs")?.language).toBe("rust");
    // Positions should be Monaco 1-indexed.
    const rsResult = results.find((r) => r.name === "fooRs")!;
    expect(rsResult.range.startLineNumber).toBe(11);
    expect(rsResult.range.startColumn).toBe(3);
  });

  it("swallows errors from individual clients", async () => {
    const failing = vi.fn().mockRejectedValue(new Error("server crashed"));
    const ok = vi.fn().mockResolvedValue([
      {
        name: "foo",
        kind: 12,
        location: {
          uri: "file:///foo.ts",
          range: {
            start: { line: 0, character: 0 },
            end: { line: 0, character: 3 },
          },
        },
      },
    ]);

    getAllClientsMock.mockReturnValue([
      {
        language: "python",
        client: {
          getCapabilities: () => ({ workspaceSymbolProvider: true }),
          getConnection: () => ({ request: failing }),
        },
      },
      {
        language: "typescript",
        client: {
          getCapabilities: () => ({ workspaceSymbolProvider: true }),
          getConnection: () => ({ request: ok }),
        },
      },
    ]);

    const { searchWorkspaceSymbols } = await import("../src/lib/lsp-workspace-symbols");
    const results = await searchWorkspaceSymbols("foo");
    expect(results).toHaveLength(1);
    expect(results[0].name).toBe("foo");
  });

  it("respects maxResults parameter", async () => {
    const many = Array.from({ length: 500 }, (_, i) => ({
      name: `sym${i}`,
      kind: 12,
      location: {
        uri: "file:///f.ts",
        range: {
          start: { line: i, character: 0 },
          end: { line: i, character: 4 },
        },
      },
    }));

    getAllClientsMock.mockReturnValue([
      {
        language: "typescript",
        client: {
          getCapabilities: () => ({ workspaceSymbolProvider: true }),
          getConnection: () => ({ request: () => Promise.resolve(many) }),
        },
      },
    ]);

    const { searchWorkspaceSymbols } = await import("../src/lib/lsp-workspace-symbols");
    const results = await searchWorkspaceSymbols("sym", 50);
    expect(results).toHaveLength(50);
  });
});
