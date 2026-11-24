import type { Organization } from "./contracts";

const prefix = "/-/organizations";
const uuid = "[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}";
const idPattern = new RegExp(`^${uuid}$`, "i");
const route = new RegExp(
  `^${prefix}/(?:[^/]+--)?(${uuid})(/invitation|/projects(?:/([0-9a-f-]{36}))?)?$`,
  "i",
);

export function matchOrganizationRoute(pathname: string) {
  return route.exec(pathname);
}

export function organizationDestination(value: string, origin: string) {
  const input = value.trim();
  if (idPattern.test(input)) return input.toLowerCase();
  if (!URL.canParse(input, origin)) return undefined;
  const url = new URL(input, origin);
  if (url.origin !== origin) return undefined;
  return matchOrganizationRoute(
    url.pathname.replace(/\/$/, ""),
  )?.[1].toLowerCase();
}

export function organizationHref(
  organization: Pick<Organization, "id" | "name">,
  section = "",
) {
  const name = organization.name
    .normalize("NFKD")
    .toLowerCase()
    .replace(/\p{Mark}/gu, "")
    .replace(/[^\p{Letter}\p{Number}]+/gu, "-");
  const slug =
    Array.from(name)
      .slice(0, 60)
      .join("")
      .replace(/^-+|-+$/g, "") || "organization";
  return `${prefix}/${encodeURIComponent(slug)}--${organization.id}${section}`;
}
