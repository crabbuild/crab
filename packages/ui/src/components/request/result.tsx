import { Button } from "@/components/ui/button";
import { Spinner } from "@/components/ui/spinner";
import { useLocation, type Loaded, type Timing } from "@/lib/api";
import type { ReactNode } from "react";

export function Result<T>({
  state,
  children,
  timingLabel,
}: {
  state: Loaded<T> & { retry: () => void };
  children: (data: T) => ReactNode;
  timingLabel?: string;
}) {
  if (state.error)
    return (
      <div className="notice error" role="alert">
        <strong>Unable to load this view</strong>
        <p>{state.error}</p>
        <Button variant="outline" onClick={state.retry}>
          Try again
        </Button>
      </div>
    );
  if (state.loading || state.data === undefined)
    return (
      <div className="notice" role="status">
        <Spinner /> Loading repository data…
      </div>
    );
  return (
    <RequestWidget timing={state.timing} label={timingLabel}>
      {children(state.data)}
    </RequestWidget>
  );
}

export function RequestWidget({
  timing,
  label: timingLabel,
  children,
}: {
  timing?: Timing;
  label?: string;
  children: ReactNode;
}) {
  const location = useLocation();
  const powerbar =
    new URL(location, window.location.origin).searchParams.get("powerbar") ===
    "1";
  const label = timingLabel ?? timing?.url;
  if (!powerbar || !label) return <>{children}</>;
  return (
    <div
      className="timing-widget"
      role="group"
      aria-label={`${label} request widget`}
    >
      {/* Keep the diagnostic header stable while a request refreshes;
          adding it after completion moves anchored menus during a click. */}
      <details className="timing" aria-label={`${label} request timing`}>
        <summary>
          <span>{label}</span>
          <strong aria-label={timing ? undefined : "No timing yet"}>
            {timing ? `${timing.roundtrip.toFixed(1)} ms` : "—"}
          </strong>
        </summary>
        <div className="timing-details">
          {timing ? (
            <>
              <code>{timing.url}</code>
              <p>
                Round trip includes the request, response transfer, and JSON
                parsing.
              </p>
              {timing.server && (
                <p>
                  Server-Timing: <code>{timing.server}</code>
                </p>
              )}
            </>
          ) : (
            <p>No completed request timing yet.</p>
          )}
        </div>
      </details>
      {children}
    </div>
  );
}
