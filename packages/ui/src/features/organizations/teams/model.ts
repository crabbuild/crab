import type { Team } from "@/lib/contracts";
export type Draft = Pick<
  Team,
  "name" | "description" | "visibility" | "version"
>;
export function teamHref(organization: string, team?: string) {
  const query = new URLSearchParams({ tab: "teams" });
  if (team) query.set("team", team);
  return `/-/organizations/${organization}?${query}`;
}
