export function parseGitRepository(value: string): string | undefined {
  const input = value.trim();
  if (!input) return undefined;
  const path = clonePath(input);
  if (!path) return undefined;
  const segments = path.split("/");
  if (
    segments.length < 2 ||
    segments.some(
      (segment) =>
        !segment ||
        segment === "." ||
        segment === ".." ||
        !/^[A-Za-z0-9_.-]{1,100}$/.test(segment),
    )
  )
    return undefined;
  return segments.at(-1);
}

function clonePath(input: string): string | undefined {
  let path = input;
  if (input.startsWith("git@")) {
    const separator = input.indexOf(":", 4);
    if (separator < 0) return undefined;
    path = input.slice(separator + 1);
  } else if (input.startsWith("github.com:")) path = input.slice(11);
  else if (input.includes("://")) {
    try {
      const url = new URL(input);
      if (
        !["https:", "ssh:"].includes(url.protocol) ||
        (url.username &&
          (url.protocol === "https:" || url.username !== "git")) ||
        url.password ||
        url.search ||
        url.hash
      )
        return undefined;
      path = url.pathname;
    } catch {
      return undefined;
    }
  }
  return path.replace(/^\/+|\/+$/g, "").replace(/\.git$/, "");
}

export function gitOwnerSuggestion(value: string | null | undefined): string {
  const owner = (value ?? "")
    .trim()
    .toLowerCase()
    .replace(/[^a-z0-9_.-]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 100);
  return owner || "imported";
}
