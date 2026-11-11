import { minimatch } from "minimatch";

/** Match repository paths using Git attribute glob syntax. */
export function matchesAny(path: string, patterns: readonly string[]): boolean {
  return patterns.some(pattern => minimatch(path, pattern.replace(/^\//, ""), {
    dot: true,
    matchBase: !pattern.includes("/"),
    nobrace: true,
    noext: true,
    nonegate: true,
    nocomment: true,
    platform: "linux",
  }));
}
