import { useEffect, useState } from "react";
import { useSettings } from "../settings";
import { useWorkspaceTheme } from "./useWorkspaceTheme";

/**
 * Resolve the effective theme into the concrete rendering mode
 * (`"dark"` or `"light"`). Precedence:
 *   workspace theme > user theme > "dark" default
 *
 * `system` follows `prefers-color-scheme` and subscribes to changes
 * so the UI repaints live when the OS flips at dusk/dawn.
 *
 * Anywhere the UI needs a Boolean "is this a dark surface?" answer —
 * Monaco themes, @pierre/diffs, SVG fills — should call this.
 */
export function useEffectiveTheme(): "dark" | "light" {
  const { settings } = useSettings();
  const { workspaceTheme } = useWorkspaceTheme();
  const [sysLight, setSysLight] = useState<boolean>(() =>
    typeof window !== "undefined" &&
    window.matchMedia("(prefers-color-scheme: light)").matches,
  );

  // The resolved theme value: workspace override wins over user setting.
  const resolvedTheme = workspaceTheme ?? settings.theme;

  useEffect(() => {
    if (resolvedTheme !== "system") return;
    const mq = window.matchMedia("(prefers-color-scheme: light)");
    const onChange = () => setSysLight(mq.matches);
    onChange();
    mq.addEventListener("change", onChange);
    return () => mq.removeEventListener("change", onChange);
  }, [resolvedTheme]);

  if (resolvedTheme === "system") return sysLight ? "light" : "dark";
  if (resolvedTheme === "hc-dark" || resolvedTheme === "dark") return "dark";
  return "light";
}
