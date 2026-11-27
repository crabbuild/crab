import {
  createFileTreeIconResolver,
  getBuiltInSpriteSheet,
} from "@pierre/trees";
import { useLayoutEffect } from "react";
import { cn } from "@/lib/utils";

const SPRITE_ELEMENT_ID = "crab-file-type-icon-sprite";
const fileIconResolver = createFileTreeIconResolver("complete");

const FILE_ICON_PALETTE_BY_TOKEN: Record<string, string> = {
  astro: "purple",
  babel: "yellow",
  bash: "green",
  biome: "blue",
  bootstrap: "indigo",
  browserslist: "yellow",
  bun: "mauve",
  c: "blue",
  cpp: "blue",
  claude: "orange",
  css: "indigo",
  database: "purple",
  default: "gray",
  docker: "blue",
  eslint: "indigo",
  font: "gray",
  git: "vermilion",
  go: "cyan",
  graphql: "pink",
  html: "orange",
  image: "pink",
  javascript: "yellow",
  json: "orange",
  markdown: "green",
  mcp: "teal",
  nextjs: "gray",
  npm: "red",
  oxc: "cyan",
  postcss: "red",
  prettier: "teal",
  python: "blue",
  react: "cyan",
  ruby: "red",
  rust: "orange",
  sass: "pink",
  stylelint: "gray",
  svg: "orange",
  svelte: "red",
  svgo: "green",
  swift: "orange",
  table: "teal",
  text: "gray",
  tailwind: "cyan",
  terraform: "indigo",
  typescript: "blue",
  vite: "purple",
  vscode: "blue",
  vue: "green",
  wasm: "indigo",
  webpack: "blue",
  yml: "red",
  zig: "orange",
  zip: "orange",
};

export interface ResolvedFileTypeIcon {
  name: string;
  token: string;
  color: string;
  width: number;
  height: number;
  viewBox: string;
}

export function resolveFileTypeIcon(path: string): ResolvedFileTypeIcon {
  const resolved = fileIconResolver.resolveIcon("file-tree-icon-file", path);
  const token = resolved.token ?? "default";
  const palette = FILE_ICON_PALETTE_BY_TOKEN[token] ?? "gray";
  return {
    name: resolved.name,
    token,
    color: `var(--file-icon-${palette})`,
    width: resolved.width ?? 16,
    height: resolved.height ?? 16,
    viewBox: resolved.viewBox ?? "0 0 16 16",
  };
}

function ensureFileIconSpriteSheet(): void {
  if (typeof document === "undefined") return;
  if (document.getElementById(SPRITE_ELEMENT_ID)) return;
  const parsed = new DOMParser()
    .parseFromString(getBuiltInSpriteSheet("complete"), "text/html")
    .body.querySelector("svg");
  if (!parsed) return;
  const sprite = document.importNode(parsed, true);
  sprite.id = SPRITE_ELEMENT_ID;
  sprite.setAttribute("aria-hidden", "true");
  sprite.setAttribute("focusable", "false");
  sprite.style.position = "absolute";
  sprite.style.width = "0";
  sprite.style.height = "0";
  sprite.style.overflow = "hidden";
  sprite.style.pointerEvents = "none";
  document.body.appendChild(sprite);
}

export function FileTypeIcon({
  path,
  className,
}: {
  path: string;
  className?: string;
}) {
  useLayoutEffect(() => {
    ensureFileIconSpriteSheet();
  }, []);

  const icon = resolveFileTypeIcon(path);
  return (
    <svg
      aria-hidden="true"
      focusable="false"
      width={icon.width}
      height={icon.height}
      viewBox={icon.viewBox}
      className={cn("file-type-icon shrink-0", className)}
      data-icon-name={icon.name}
      data-icon-token={icon.token}
      style={{ color: icon.color }}
    >
      <use href={`#${icon.name}`} />
    </svg>
  );
}
