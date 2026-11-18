import { useEffect, useState } from "react";
import type { TokenEventBase } from "@pierre/diffs";

export type CodeSymbolLanguage =
  | "c"
  | "cpp"
  | "go"
  | "java"
  | "javascript"
  | "python"
  | "rust"
  | "typescript";
export type CodeSymbolRole = "definition" | "reference";

export type CodeSymbol = {
  id: string;
  name: string;
  role: CodeSymbolRole;
  kind: string;
  line: number;
  column: number;
  endColumn: number;
  container?: string;
};

export type CodeSymbolsState =
  | { status: "idle"; symbols: CodeSymbol[] }
  | { status: "loading"; symbols: CodeSymbol[] }
  | { status: "ready"; symbols: CodeSymbol[] }
  | { status: "error"; symbols: CodeSymbol[]; message: string };

type WorkerResponse =
  | { requestId: number; symbols: CodeSymbol[] }
  | { requestId: number; error: string };

export function codeSymbolLanguage(name: string): CodeSymbolLanguage | null {
  const extension = name.toLowerCase().split(".").pop();
  if (extension === "c" || extension === "h") return "c";
  if (
    ["cc", "cp", "cpp", "cxx", "c++", "hh", "hpp", "hxx", "h++"].includes(
      extension ?? "",
    )
  ) {
    return "cpp";
  }
  if (extension === "go") return "go";
  if (extension === "java") return "java";
  if (["py", "pyi", "pyw"].includes(extension ?? "")) return "python";
  if (extension === "rs") return "rust";
  if (["ts", "tsx", "mts", "cts"].includes(extension ?? "")) {
    return "typescript";
  }
  if (["js", "jsx", "mjs", "cjs"].includes(extension ?? "")) {
    return "javascript";
  }
  return null;
}

export function findSymbolAt(
  symbols: CodeSymbol[],
  token: TokenEventBase,
  point: Pick<MouseEvent, "clientX" | "clientY">,
) {
  const candidates = symbols.filter(
    (symbol) =>
      symbol.line === token.lineNumber &&
      token.lineCharStart < symbol.endColumn &&
      token.lineCharEnd > symbol.column,
  );
  if (candidates.length === 0) return;
  // A syntax token can contain an entire expression before colors load.
  // Hit-test captured names so clicking a method cannot select its receiver.
  const walker = document.createTreeWalker(
    token.tokenElement,
    NodeFilter.SHOW_TEXT,
  );
  const range = document.createRange();
  let column = token.lineCharStart;
  for (let node = walker.nextNode(); node; node = walker.nextNode()) {
    const length = node.textContent?.length ?? 0;
    for (const symbol of candidates) {
      const start = Math.max(0, symbol.column - column);
      const end = Math.min(length, symbol.endColumn - column);
      if (start >= end) continue;
      range.setStart(node, start);
      range.setEnd(node, end);
      if (
        [...range.getClientRects()].some(
          (rect) =>
            point.clientX >= rect.left &&
            point.clientX <= rect.right &&
            point.clientY >= rect.top &&
            point.clientY <= rect.bottom,
        )
      )
        return symbol;
    }
    column += length;
  }
}

export function useCodeSymbols(
  name: string,
  source: string | null | undefined,
): CodeSymbolsState {
  const language = codeSymbolLanguage(name);
  const [state, setState] = useState<CodeSymbolsState>({
    status: "idle",
    symbols: [],
  });

  useEffect(() => {
    if (language === null || source === null || source === undefined) {
      setState({ status: "idle", symbols: [] });
      return;
    }

    const worker = new Worker(new URL("./worker.ts", import.meta.url), {
      type: "module",
    });
    const requestId = 1;
    setState({ status: "loading", symbols: [] });
    worker.onmessage = (event: MessageEvent<WorkerResponse>) => {
      if (event.data.requestId !== requestId) return;
      worker.terminate();
      if ("error" in event.data) {
        setState({
          status: "error",
          symbols: [],
          message: event.data.error,
        });
        return;
      }
      setState({ status: "ready", symbols: event.data.symbols });
    };
    worker.onerror = () => {
      worker.terminate();
      setState({
        status: "error",
        symbols: [],
        message: "The browser parser could not be loaded.",
      });
    };
    worker.postMessage({ requestId, language, source });
    return () => worker.terminate();
  }, [language, source]);

  return state;
}
