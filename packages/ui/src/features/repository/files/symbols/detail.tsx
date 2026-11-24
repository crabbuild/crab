import { Button } from "@/components/ui/button";
import {
  getFiletypeFromFileName,
  getSharedHighlighter,
  type DiffsThemeNames,
  type FileContents,
  type ThemedToken,
} from "@pierre/diffs";
import { useEffect, useMemo, useState } from "react";
import type { CodeSymbol } from ".";

function SymbolLine({
  source,
  tokens,
  matches,
}: {
  source: string;
  tokens: ThemedToken[] | undefined;
  matches: CodeSymbol[];
}) {
  const indentation = source.length - source.trimStart().length;
  let column = 0;
  return (
    <code>
      {(tokens ?? [{ content: source, offset: 0 }]).map((token, index) => {
        const tokenStart = column;
        const end = (column += token.content.length);
        const start = Math.max(tokenStart, indentation);
        if (start >= end) return null;
        const boundaries = matches.flatMap((match) => [
          match.column,
          match.endColumn,
        ]);
        const cuts = [
          ...new Set([
            start,
            end,
            ...boundaries.filter((offset) => offset > start && offset < end),
          ]),
        ].sort((a, b) => a - b);
        return (
          <span key={index} style={{ color: token.color }}>
            {cuts.slice(0, -1).map((from, position) => {
              const to = cuts[position + 1];
              const text = token.content.slice(
                from - tokenStart,
                to - tokenStart,
              );
              return matches.some(
                (match) => match.column <= from && match.endColumn >= to,
              ) ? (
                <mark key={from}>{text}</mark>
              ) : (
                text
              );
            })}
          </span>
        );
      })}
    </code>
  );
}

export function CodeSymbolDetails({
  symbols,
  active,
  file,
  theme,
  onActivate,
}: {
  symbols: CodeSymbol[];
  active: CodeSymbol;
  file: FileContents;
  theme: DiffsThemeNames;
  onActivate: (symbol: CodeSymbol) => void;
}) {
  const [highlight, setHighlight] = useState<{
    file: FileContents;
    theme: DiffsThemeNames;
    tokens: ThemedToken[][] | null;
  } | null>(null);
  const tokens =
    highlight?.file === file && highlight.theme === theme
      ? highlight.tokens
      : undefined;
  const lines = useMemo(() => file.contents.split(/\r?\n/), [file.contents]);
  const matchesByLine = useMemo(() => {
    const groups = new Map<number, CodeSymbol[]>();
    for (const symbol of symbols) {
      const matches = groups.get(symbol.line) ?? [];
      matches.push(symbol);
      groups.set(symbol.line, matches);
    }
    return groups;
  }, [symbols]);
  // Following a reference keeps the definition's kind in the focused header.
  const focused =
    symbols.find((symbol) => symbol.role === "definition") ?? active;

  useEffect(() => {
    let cancelled = false;
    const lang = file.lang ?? getFiletypeFromFileName(file.name);
    // Highlight the whole file once so multiline syntax keeps its context.
    // Reference clicks reuse these tokens and the source viewer's highlighter.
    getSharedHighlighter({ themes: [theme], langs: [lang] })
      .then((highlighter) => {
        if (cancelled) return;
        const { tokens } = highlighter.codeToTokens(file.contents, {
          lang,
          theme,
        });
        setHighlight({ file, theme, tokens });
      })
      .catch(() => {
        if (!cancelled) setHighlight({ file, theme, tokens: null });
      });
    return () => {
      cancelled = true;
    };
  }, [file, theme]);

  return (
    <div className="code-symbol-detail">
      <div className="code-symbol-detail-title">
        <span>{focused.kind}</span>
        <strong>{focused.name}</strong>
      </div>
      {tokens === null && (
        <p className="code-symbols-status" role="status">
          Syntax colors unavailable.
        </p>
      )}
      {(["definition", "reference"] as const).map((role) => {
        const occurrences = symbols.filter((symbol) => symbol.role === role);
        // Share one preview per line while counting every captured occurrence.
        const rows = [
          ...new Map(
            occurrences.map((symbol) => [symbol.line, symbol]),
          ).values(),
        ];
        return (
          <section className="code-symbol-group" key={role}>
            <h3>
              {role === "definition" ? "Definitions" : "References"}{" "}
              <span>{occurrences.length}</span>
            </h3>
            {rows.length === 0 ? (
              <p className="code-symbol-empty">None in this file</p>
            ) : (
              <>
                <p className="code-symbol-scope">In this file</p>
                {rows.map((symbol) => {
                  const source = lines[symbol.line - 1];
                  const current =
                    symbol.role === active.role && symbol.line === active.line;
                  return (
                    <Button
                      variant="ghost"
                      className="code-symbol-row"
                      type="button"
                      key={symbol.id}
                      aria-label={`${symbol.name} ${symbol.kind} ${symbol.line}`}
                      aria-current={current ? "location" : undefined}
                      title={source.trim()}
                      onClick={() => onActivate(symbol)}
                    >
                      <span className="code-symbol-line">{symbol.line}</span>
                      <SymbolLine
                        source={source}
                        tokens={tokens?.[symbol.line - 1]}
                        matches={matchesByLine.get(symbol.line) ?? []}
                      />
                    </Button>
                  );
                })}
              </>
            )}
          </section>
        );
      })}
    </div>
  );
}
