import { Component } from "react";
import type { AnchorHTMLAttributes, ReactNode } from "react";
import { Button, Spinner } from "@primer/react";
import { navigate, type Loaded } from "./api";

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
  showTiming = true,
}: {
  state: Loaded<T> & { retry: () => void };
  children: (data: T) => ReactNode;
  showTiming?: boolean;
}) {
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
  return (
    <>
      {children(state.data)}
      {showTiming && state.timing && (
        <details className="timing">
          <summary>Request timing</summary>
          <div title={state.timing.server ?? undefined}>
            {state.timing.roundtrip.toFixed(1)} ms round trip{" "}
            <span aria-hidden="true">·</span>{" "}
            <span>Object storage → browser</span>
          </div>
        </details>
      )}
    </>
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
