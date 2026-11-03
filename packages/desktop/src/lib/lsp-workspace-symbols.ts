/**
 * Workspace symbol search via LSP.
 *
 * Fetches symbols across the whole project from any running LSP client
 * that advertises `workspaceSymbolProvider`. Used by the command palette
 * for "Go to Symbol in Workspace" (Cmd/Ctrl + T in VS Code).
 */

import { lspRegistry } from "./lsp-registry";
import type { LspWorkspaceSymbol, LspLocation } from "./lsp-types";
import { lspToMonacoRange } from "./lsp-converters";

export interface WorkspaceSymbolResult {
  /** Symbol name (e.g., function name). */
  name: string;
  /** LSP SymbolKind (Function=12, Class=5, etc.). */
  kind: number;
  /** Containing symbol (e.g., class for a method). */
  containerName?: string;
  /** Language of the server that produced this result. */
  language: string;
  /** File URI where the symbol lives. */
  uri: string;
  /** Range of the symbol in the file (0-indexed LSP → 1-indexed Monaco). */
  range: {
    startLineNumber: number;
    startColumn: number;
    endLineNumber: number;
    endColumn: number;
  };
}

/**
 * Search workspace symbols across all running LSP clients.
 *
 * Queries every client that supports `workspaceSymbolProvider` in
 * parallel and aggregates results. Individual failures are swallowed —
 * if one server errors, results from others still surface.
 */
export async function searchWorkspaceSymbols(
  query: string,
  maxResults = 100,
): Promise<WorkspaceSymbolResult[]> {
  if (!query.trim()) return [];

  const clients = lspRegistry.getAllClients();
  if (clients.length === 0) return [];

  const promises = clients
    .filter((c) => c.client.getCapabilities().workspaceSymbolProvider)
    .map(async ({ language, client }) => {
      try {
        const result = await client.getConnection().request(
          "workspace/symbol",
          { query },
          5000,
        ) as LspWorkspaceSymbol[] | null;

        if (!result || !Array.isArray(result)) return [];

        return result.map((sym): WorkspaceSymbolResult => {
          // Location can be a full LspLocation or a simplified { uri }
          // with the range embedded in `location.range` (rare).
          const loc = sym.location as LspLocation;
          const range = loc.range
            ? lspToMonacoRange(loc.range)
            : {
                startLineNumber: 1,
                startColumn: 1,
                endLineNumber: 1,
                endColumn: 1,
              };
          return {
            name: sym.name,
            kind: sym.kind,
            containerName: sym.containerName,
            language,
            uri: loc.uri,
            range,
          };
        });
      } catch {
        // Server error — skip this client, let others contribute
        return [];
      }
    });

  const results = (await Promise.all(promises)).flat();
  return results.slice(0, maxResults);
}
