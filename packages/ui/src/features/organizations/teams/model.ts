import type { Team } from "@/lib/contracts";
export type Draft = Pick<
  Team,
  "name" | "description" | "visibility" | "version"
>;
export function teamHref(
  organization: string,
  team?: string,
  view?: "repositories" | "settings",
) {
  const query = new URLSearchParams({ tab: "teams" });
  if (team) query.set("team", team);
  if (view) query.set("view", view);
  return `/-/organizations/${organization}?${query}`;
}
