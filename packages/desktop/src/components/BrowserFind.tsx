import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { ChevronDown, ChevronUp, X } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";

interface BrowserFindProps {
  isOpen: boolean;
  onClose: () => void;
  webviewRef: { current: Electron.WebviewTag | null };
}

export function BrowserFind({ isOpen, onClose, webviewRef }: BrowserFindProps) {
  const inputRef = useRef<HTMLInputElement>(null);
  const wasOpenRef = useRef(isOpen);
  const [query, setQuery] = useState("");
  const [activeMatch, setActiveMatch] = useState(0);
  const [totalMatches, setTotalMatches] = useState(0);

  const safeFindInPage = useCallback(
    (text: string, opts?: Electron.FindInPageOptions) => {
      const wv = webviewRef.current;
      if (!wv || !text) return;
      try {
        wv.findInPage(text, opts);
      } catch {
        // webview may be mid-teardown
      }
    },
    [webviewRef]
  );

  const safeStopFindInPage = useCallback(() => {
    const wv = webviewRef.current;
    if (!wv) return;
    try {
      wv.stopFindInPage("clearSelection");
    } catch {
      // best-effort
    }
  }, [webviewRef]);

  const findNext = useCallback(() => {
    if (query) {
      safeFindInPage(query, { forward: true, findNext: true });
    }
  }, [query, safeFindInPage]);

  const findPrevious = useCallback(() => {
    if (query) {
      safeFindInPage(query, { forward: false, findNext: true });
    }
  }, [query, safeFindInPage]);

  useEffect(() => {
    if (isOpen) {
      inputRef.current?.focus();
      inputRef.current?.select();
    } else {
      safeStopFindInPage();
      setActiveMatch(0);
      setTotalMatches(0);
    }
  }, [isOpen, safeStopFindInPage]);

  useEffect(() => {
    const wasOpen = wasOpenRef.current;
    wasOpenRef.current = isOpen;
    if (!isOpen) return;

    if (!query) {
      safeStopFindInPage();
      setActiveMatch(0);
      setTotalMatches(0);
      return;
    }

    const runFind = () => safeFindInPage(query);
    if (!wasOpen) {
      runFind();
      return;
    }
    // Debounce typing
    const id = window.setTimeout(runFind, 200);
    return () => window.clearTimeout(id);
  }, [isOpen, query, safeFindInPage, safeStopFindInPage]);

  useEffect(() => {
    const wv = webviewRef.current;
    if (!wv || !isOpen) return;

    const handleFoundInPage = (event: Electron.FoundInPageEvent) => {
      setActiveMatch(event.result.activeMatchOrdinal);
      setTotalMatches(event.result.matches);
    };

    wv.addEventListener("found-in-page", handleFoundInPage);
    return () => {
      try {
        wv.removeEventListener("found-in-page", handleFoundInPage);
      } catch {
        // webview may be destroyed
      }
    };
  }, [webviewRef, isOpen]);

  const handleKeyDown = useCallback(
    (e: React.KeyboardEvent) => {
      e.stopPropagation();
      if (e.key === "Escape") {
        onClose();
      } else if (e.key === "Enter" && e.shiftKey) {
        findPrevious();
      } else if (e.key === "Enter") {
        findNext();
      }
    },
    [onClose, findNext, findPrevious]
  );

  if (!isOpen) return null;

  return (
    <div
      className="absolute top-3 right-3 z-50 flex items-center gap-1.5 rounded-lg border border-border bg-card/95 px-2.5 py-1.5 shadow-lg backdrop-blur-sm"
      style={{ width: 320 }}
      onKeyDown={handleKeyDown}
    >
      <Input
        ref={inputRef}
        type="text"
        value={query}
        onChange={(e) => setQuery(e.target.value)}
        placeholder="Find in page..."
        className="min-w-0 flex-1 border-none bg-transparent text-sm text-foreground outline-none placeholder-muted-foreground/60"
      />

      {query ? (
        <span className="shrink-0 text-[11px] text-muted-foreground">
          {totalMatches > 0 ? `${activeMatch} of ${totalMatches}` : "No matches"}
        </span>
      ) : null}

      <div className="mx-0.5 h-4 w-px bg-border" />

      <Button
        variant="ghost"
        size="compact"
        type="button"
        className="flex size-6 shrink-0 items-center justify-center rounded text-muted-foreground hover:text-foreground transition"
        onClick={findPrevious}
        title="Previous match"
      >
        <ChevronUp size={14} />
      </Button>

      <Button
        variant="ghost"
        size="compact"
        type="button"
        className="flex size-6 shrink-0 items-center justify-center rounded text-muted-foreground hover:text-foreground transition"
        onClick={findNext}
        title="Next match"
      >
        <ChevronDown size={14} />
      </Button>

      <div className="mx-0.5 h-4 w-px bg-border" />

      <Button
        variant="ghost"
        size="compact"
        type="button"
        className="flex size-6 shrink-0 items-center justify-center rounded text-muted-foreground hover:text-foreground transition"
        onClick={onClose}
        title="Close"
      >
        <X size={14} />
      </Button>
    </div>
  );
}
