import { Component } from "react";
import type { AnchorHTMLAttributes, ReactNode } from "react";
import { Button, Spinner } from "@primer/react";
import { navigate, useLocation, type Loaded } from "./api";

export function Link({
  href = "/",
  onClick,
  ...props
}: AnchorHTMLAttributes<HTMLAnchorElement>) {
  return (
    <a
      {...props}
      href={href}
      onClick={(event) => {
        onClick?.(event);
        if (
          !event.defaultPrevented &&
          event.button === 0 &&
          !event.metaKey &&
          !event.ctrlKey &&
          !event.shiftKey &&
          !event.altKey &&
          !props.target
        ) {
          event.preventDefault();
          navigate(href);
        }
      }}
    />
  );
}
export function Result<T>({
  state,
  children,
  timingLabel,
}: {
  state: Loaded<T> & { retry: () => void };
  children: (data: T) => ReactNode;
  timingLabel?: string;
}) {
  const location = useLocation();
  const powerbar =
    new URL(location, window.location.origin).searchParams.get("powerbar") ===
    "1";
  if (state.error)
    return (
      <div className="notice error" role="alert">
        <strong>Unable to load this view</strong>
        <p>{state.error}</p>
        <Button onClick={state.retry}>Try again</Button>
      </div>
    );
  if (state.loading || state.data === undefined)
    return (
      <div className="notice" role="status">
        <Spinner size="small" /> Loading repository data…
      </div>
    );
  const content = children(state.data);
  if (!powerbar || !state.timing) return <>{content}</>;
  const label = timingLabel ?? state.timing.url;
  return (
    <div
      className="timing-widget"
      role="group"
      aria-label={`${label} request widget`}
    >
      <details className="timing" aria-label={`${label} request timing`}>
        <summary>
          <span>{label}</span>
          <strong>{state.timing.roundtrip.toFixed(1)} ms</strong>
        </summary>
        <div className="timing-details">
          <code>{state.timing.url}</code>
          <p>
            Round trip includes the request, response transfer, and JSON
            parsing.
          </p>
          {state.timing.server && (
            <p>
              Server-Timing: <code>{state.timing.server}</code>
            </p>
          )}
        </div>
      </details>
      {content}
    </div>
  );
}
export function short(oid: string) {
  return oid.slice(0, 7);
}
export function date(seconds: number) {
  return new Date(seconds * 1000).toLocaleDateString(undefined, {
    year: "numeric",
    month: "short",
    day: "numeric",
  });
}
export function relativeDate(seconds: number) {
  const elapsed = seconds - Date.now() / 1000;
  const intervals: Array<[Intl.RelativeTimeFormatUnit, number]> = [
    ["year", 365 * 24 * 60 * 60],
    ["month", 30 * 24 * 60 * 60],
    ["week", 7 * 24 * 60 * 60],
    ["day", 24 * 60 * 60],
    ["hour", 60 * 60],
    ["minute", 60],
  ];
  const formatter = new Intl.RelativeTimeFormat(undefined, { numeric: "auto" });
  for (const [unit, duration] of intervals) {
    if (Math.abs(elapsed) >= duration)
      return formatter.format(Math.round(elapsed / duration), unit);
  }
  return formatter.format(Math.round(elapsed), "second");
}

export class AppErrorBoundary extends Component<
  { children: ReactNode },
  { failed: boolean }
> {
  state = { failed: false };
  static getDerivedStateFromError() {
    return { failed: true };
  }
  render() {
    if (this.state.failed)
      return (
        <main className="notice" role="alert">
          <h1>The repository viewer could not load</h1>
          <p>Reload to fetch the current application and try again.</p>
          <button onClick={() => window.location.reload()}>
            Reload application
          </button>
        </main>
      );
    return this.props.children;
  }
}
