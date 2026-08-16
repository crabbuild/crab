import type { CSSProperties } from "react";

/**
 * Shared loading fallback for marketing route transitions.
 *
 * Server Component (no client JS). Renders a low-key placeholder that fades
 * in (opacity 0 → 1) over 300ms via the `animate-fade-in` utility defined
 * in `app/globals.css`. The element reserves vertical space so the layout
 * does not collapse during the brief streaming window between route segments.
 *
 * Respects `prefers-reduced-motion` — the keyframe is wrapped in a media
 * query in `globals.css` so users who opt out see the final state without
 * animation.
 */
export function MarketingLoading() {
  // The pulse on the inner skeleton uses Tailwind's built-in `animate-pulse`
  // utility; the outer fade is the page transition animation.
  const wrapperStyle: CSSProperties = {
    minHeight: "60vh",
  };

  return (
    <div
      role="status"
      aria-label="Loading"
      aria-busy="true"
      className="animate-fade-in flex w-full items-center justify-center px-6 py-24"
      style={wrapperStyle}
    >
      <div className="w-full max-w-3xl space-y-6">
        <div className="bg-muted h-10 w-3/4 animate-pulse rounded-md" />
        <div className="bg-muted h-4 w-full animate-pulse rounded-md" />
        <div className="bg-muted h-4 w-5/6 animate-pulse rounded-md" />
        <div className="bg-muted mt-8 h-48 w-full animate-pulse rounded-xl" />
      </div>
      <span className="sr-only">Loading page content</span>
    </div>
  );
}

export default MarketingLoading;
