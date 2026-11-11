import { lazy, Suspense, useMemo } from "react";
import { useEffectiveTheme } from "../../hooks/useEffectiveTheme";
import { diffOptions } from "../../lib/diffTheme";

const SafePatchDiff = lazy(() =>
  import("../SafePatchDiff").then((m) => ({ default: m.SafePatchDiff })),
);

/**
 * Skeleton placeholder matching the approximate height of a small
 * diff (~120 px) so layout doesn't jump when SafePatchDiff resolves.
 */
function DiffSkeleton() {
  return (
    <div
      className="animate-pulse rounded bg-card/40"
      style={{ height: 120 }}
      aria-busy="true"
      aria-label="Loading diff…"
    />
  );
}

/**
 * Unified entry point for rendering diffs across the chat surface.
 *
 * EditCard, TurnSummaryCard, and ToolGroup all share this renderer
 * so diffs look identical everywhere. Lazy-loads SafePatchDiff
 * because @pierre/diffs' worker pool isn't tiny and we don't want
 * to pay for it on chat-only sessions where no diff is expanded.
 */
export function UnifiedDiff({
  patch,
  language: _language,
}: {
  patch: string;
  language?: string;
}): JSX.Element {
  const effective = useEffectiveTheme();
  const options = useMemo(() => diffOptions(effective), [effective]);

  return (
    <Suspense fallback={<DiffSkeleton />}>
      <SafePatchDiff patch={patch} options={options} />
    </Suspense>
  );
}
