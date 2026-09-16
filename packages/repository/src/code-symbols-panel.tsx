import { useDeferredValue, useEffect, useMemo, useRef, useState } from "react";
import { IconButton, Spinner } from "@primer/react";
import {
  ChevronLeftIcon,
  CodeIcon,
  SearchIcon,
  XIcon,
} from "@primer/octicons-react";
import type { CodeSymbol, CodeSymbolsState } from "./code-symbols";

type Props = {
  state: CodeSymbolsState;
  active: CodeSymbol | null;
  onActivate: (symbol: CodeSymbol) => void;
  onClearActive: () => void;
  onClose: () => void;
};

function SymbolRow({
  symbol,
  onActivate,
}: {
  symbol: CodeSymbol;
  onActivate: (symbol: CodeSymbol) => void;
}) {
  return (
    <button
      className="code-symbol-row"
      type="button"
      onClick={() => onActivate(symbol)}
      title={`${symbol.kind} ${symbol.name}, line ${symbol.line}`}
    >
      <CodeIcon size={14} aria-hidden="true" />
      <span className="code-symbol-name">{symbol.name}</span>
      <span className="code-symbol-kind">{symbol.kind}</span>
      <span className="code-symbol-line">{symbol.line}</span>
    </button>
  );
}

export function CodeSymbolsPanel({
  state,
  active,
  onActivate,
  onClearActive,
  onClose,
}: Props) {
  const panel = useRef<HTMLElement>(null);
  const [filter, setFilter] = useState("");
  const deferredFilter = useDeferredValue(filter.trim().toLowerCase());
  const definitions = useMemo(
    () =>
      state.symbols.filter(
        (symbol) =>
          symbol.role === "definition" &&
          (deferredFilter === "" ||
            symbol.name.toLowerCase().includes(deferredFilter) ||
            symbol.kind.toLowerCase().includes(deferredFilter) ||
            symbol.container?.toLowerCase().includes(deferredFilter)),
      ),
    [deferredFilter, state.symbols],
  );
  const related = useMemo(
    () =>
      active === null
        ? []
        : state.symbols.filter((symbol) => symbol.name === active.name),
    [active, state.symbols],
  );
  const groups = useMemo(() => {
    const grouped = new Map<string, CodeSymbol[]>();
    for (const symbol of definitions) {
      const key = symbol.container ?? "File scope";
      grouped.set(key, [...(grouped.get(key) ?? []), symbol]);
    }
    return [...grouped];
  }, [definitions]);

  useEffect(() => {
    let frame = 0;
    const fitToViewport = () => {
      cancelAnimationFrame(frame);
      frame = requestAnimationFrame(() => {
        const node = panel.current;
        if (!node) return;
        // The pane grows as it becomes sticky; a fixed viewport fraction either
        // truncates tall screens or overflows below the fold before it sticks.
        const top = Math.max(0, node.getBoundingClientRect().top);
        node.style.setProperty(
          "--code-symbols-available-height",
          `${window.innerHeight - top}px`,
        );
      });
    };
    fitToViewport();
    window.addEventListener("resize", fitToViewport);
    window.addEventListener("scroll", fitToViewport, { passive: true });
    return () => {
      cancelAnimationFrame(frame);
      window.removeEventListener("resize", fitToViewport);
      window.removeEventListener("scroll", fitToViewport);
    };
  }, []);

  return (
    <aside ref={panel} className="code-symbols-panel" aria-label="Code symbols">
      <header className="code-symbols-header">
        <div>
          <strong>Symbols</strong>
          <span>Current file</span>
        </div>
        <IconButton
          icon={XIcon}
          size="small"
          variant="invisible"
          aria-label="Close symbols"
          onClick={onClose}
        />
      </header>
      {active === null ? (
        <>
          <label className="code-symbols-search">
            <SearchIcon size={16} aria-hidden="true" />
            <span className="sr-only">Filter symbols</span>
            <input
              value={filter}
              onChange={(event) => setFilter(event.target.value)}
              placeholder="Filter symbols"
            />
          </label>
          <div className="code-symbols-list" aria-live="polite">
            {state.status === "loading" ? (
              <div className="code-symbols-status">
                <Spinner size="small" /> Parsing in your browser…
              </div>
            ) : state.status === "error" ? (
              <div className="code-symbols-status" title={state.message}>
                Symbols unavailable for this file.
              </div>
            ) : groups.length === 0 ? (
              <div className="code-symbols-status">
                {filter ? "No matching symbols." : "No symbols found."}
              </div>
            ) : (
              groups.map(([container, symbols]) => (
                <section className="code-symbol-group" key={container}>
                  <h3>{container}</h3>
                  {symbols.map((symbol) => (
                    <SymbolRow
                      key={symbol.id}
                      symbol={symbol}
                      onActivate={onActivate}
                    />
                  ))}
                </section>
              ))
            )}
          </div>
        </>
      ) : (
        <div className="code-symbol-detail">
          <button
            className="code-symbol-back"
            type="button"
            onClick={onClearActive}
          >
            <ChevronLeftIcon size={16} aria-hidden="true" /> All symbols
          </button>
          <div className="code-symbol-detail-title">
            <CodeIcon size={16} aria-hidden="true" />
            <strong>{active.name}</strong>
            <span>{active.kind}</span>
          </div>
          {(["definition", "reference"] as const).map((role) => {
            const symbols = related.filter((symbol) => symbol.role === role);
            return (
              <section className="code-symbol-group" key={role}>
                <h3>
                  {role === "definition" ? "Definitions" : "References"}{" "}
                  <span>{symbols.length}</span>
                </h3>
                {symbols.length === 0 ? (
                  <p className="code-symbol-empty">None in this file</p>
                ) : (
                  symbols.map((symbol) => (
                    <SymbolRow
                      key={symbol.id}
                      symbol={symbol}
                      onActivate={onActivate}
                    />
                  ))
                )}
              </section>
            );
          })}
        </div>
      )}
    </aside>
  );
}
