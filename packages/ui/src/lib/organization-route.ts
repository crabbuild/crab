import type { Organization } from "./contracts";

const prefix = "/-/organizations";
const uuid = "[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}";
const idPattern = new RegExp(`^${uuid}$`, "i");
const handlePattern = /^[a-z0-9](?:[a-z0-9-]{0,37}[a-z0-9])?$/;
const reservedHandles = new Set(["api", "assets", "auth", "git", "local"]);
const route = new RegExp(
  `^${prefix}/(?:[^/]+--)?(${uuid})(/invitation|/projects(?:/([0-9a-f-]{36}))?)?$`,
  "i",
);

export function validOrganizationHandle(handle: string): boolean {
  const normalized = handle.trim().toLowerCase();
  return (
    handlePattern.test(normalized) &&
    !normalized.includes("--") &&
    !reservedHandles.has(normalized) &&
    !/^(org|user)-[0-9a-f]{32}$/.test(normalized)
  );
}

export function matchOrganizationRoute(pathname: string) {
  return route.exec(pathname);
}

export function matchOrganizationHandleRoute(
  pathname: string,
): string | undefined {
  const trimmed = pathname.replace(/\/$/, "");
  const single = /^\/([^/]+)$/.exec(trimmed);
  if (!single) return undefined;
  const candidate = single[1].toLowerCase();
  return validOrganizationHandle(candidate) ? candidate : undefined;
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
  organization: Pick<Organization, "id" | "name"> & { handle?: string },
  section = "",
) {
  const handle = organization.handle?.trim().toLowerCase();
  if (
    handle &&
    validOrganizationHandle(handle) &&
    (section === "" || section.startsWith("?"))
  ) {
    return `/${encodeURIComponent(handle)}${section}`;
  }
  const slugSource =
    handle ||
    organization.name
      .normalize("NFKD")
      .toLowerCase()
      .replace(/\p{Mark}/gu, "")
      .replace(/[^\p{Letter}\p{Number}]+/gu, "-")
      .replace(/^-+|-+$/g, "") ||
    "organization";
  return `${prefix}/${encodeURIComponent(slugSource)}--${organization.id}${section}`;
}
