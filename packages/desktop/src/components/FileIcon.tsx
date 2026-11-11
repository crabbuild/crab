import {
  FileCode,
  FileJson,
  FileText,
  FileImage,
  FileArchive,
  File as FileIconLucide,
  Binary,
  FileVideo,
  FileAudio,
  FileSpreadsheet,
  Database,
  Settings,
  Shield,
  type LucideIcon,
} from "lucide-react";
import { EntryKind } from "../ipc";

/**
 * Icon + colour mapping per extension. Each entry specifies the Lucide
 * icon and a Tailwind colour class so different file types are visually
 * distinct in both the file tree and the tab bar — similar to VS Code's
 * file-icon themes.
 */
interface IconDef {
  icon: LucideIcon;
  color: string;
}

/**
 * Colour palette aligned with @pierre/trees built-in icon colors.
 * The tree component uses `light-dark()` CSS for its Shadow DOM icons;
 * we use Tailwind `dark:` variants to achieve the same effect in React.
 *
 * Mapping from @pierre/trees BUILT_IN_FILE_ICON_COLOR_FALLBACKS:
 *   typescript/python/webpack → blue (#1a85d4 / #69b1ff)
 *   javascript/browserslist   → yellow (#d5a910 / #ffd452)
 *   markdown/vue/bash/svgo    → green (#199f43 / #5ecc71)
 *   rust/json/html/swift/zip  → orange (#d47628 / #ffa359)
 *   css/eslint/terraform/wasm → purple (#693acf / #9d6afb)
 *   graphql/image/sass/bun    → pink (#d32a61 / #ff678d)
 *   go/react/tailwind/prettier→ cyan (#1ca1c7 / #68cdf2)
 *   ruby/npm/postcss/yml      → red (#d52c36 / #ff6762)
 *   default/text              → slate (#84848a / #adadb1)
 */
const C = {
  blue: "text-[#1a85d4] dark:text-[#69b1ff]",
  yellow: "text-[#d5a910] dark:text-[#ffd452]",
  green: "text-[#199f43] dark:text-[#5ecc71]",
  orange: "text-[#d47628] dark:text-[#ffa359]",
  purple: "text-[#693acf] dark:text-[#9d6afb]",
  pink: "text-[#d32a61] dark:text-[#ff678d]",
  cyan: "text-[#1ca1c7] dark:text-[#68cdf2]",
  red: "text-[#d52c36] dark:text-[#ff6762]",
  slate: "text-[#84848a] dark:text-[#adadb1]",
} as const;

const EXT_ICON: Record<string, IconDef> = {
  // TypeScript / JavaScript
  ts: { icon: FileCode, color: C.blue },
  tsx: { icon: FileCode, color: C.blue },
  js: { icon: FileCode, color: C.yellow },
  jsx: { icon: FileCode, color: C.yellow },
  mjs: { icon: FileCode, color: C.yellow },
  cjs: { icon: FileCode, color: C.yellow },

  // Python
  py: { icon: FileCode, color: C.green },
  pyi: { icon: FileCode, color: C.green },
  ipynb: { icon: FileCode, color: C.orange },

  // Rust
  rs: { icon: FileCode, color: C.orange },

  // Go
  go: { icon: FileCode, color: C.cyan },

  // Java / Kotlin / Scala
  java: { icon: FileCode, color: C.red },
  kt: { icon: FileCode, color: C.purple },
  scala: { icon: FileCode, color: C.red },

  // C / C++
  c: { icon: FileCode, color: C.blue },
  cpp: { icon: FileCode, color: C.blue },
  cc: { icon: FileCode, color: C.blue },
  h: { icon: FileCode, color: C.purple },
  hpp: { icon: FileCode, color: C.purple },

  // C#
  cs: { icon: FileCode, color: C.green },

  // Ruby
  rb: { icon: FileCode, color: C.red },

  // PHP
  php: { icon: FileCode, color: C.purple },

  // Swift
  swift: { icon: FileCode, color: C.orange },

  // Shell
  sh: { icon: FileCode, color: C.slate },
  bash: { icon: FileCode, color: C.slate },
  zsh: { icon: FileCode, color: C.slate },

  // Web
  html: { icon: FileCode, color: C.orange },
  htm: { icon: FileCode, color: C.orange },
  css: { icon: FileCode, color: C.blue },
  scss: { icon: FileCode, color: C.pink },
  sass: { icon: FileCode, color: C.pink },
  less: { icon: FileCode, color: C.blue },
  vue: { icon: FileCode, color: C.green },
  svelte: { icon: FileCode, color: C.orange },

  // Data / Config — JSON
  json: { icon: FileJson, color: C.yellow },
  jsonl: { icon: FileJson, color: C.yellow },
  jsonc: { icon: FileJson, color: C.yellow },

  // Data / Config — YAML / TOML / INI
  yaml: { icon: Settings, color: C.purple },
  yml: { icon: Settings, color: C.purple },
  toml: { icon: Settings, color: C.slate },
  ini: { icon: Settings, color: C.slate },
  env: { icon: Shield, color: C.yellow },

  // Markdown / Text / Docs
  md: { icon: FileText, color: C.blue },
  markdown: { icon: FileText, color: C.blue },
  mdx: { icon: FileText, color: C.blue },
  txt: { icon: FileText, color: C.slate },
  rst: { icon: FileText, color: C.slate },
  log: { icon: FileText, color: C.slate },

  // Images
  png: { icon: FileImage, color: C.green },
  jpg: { icon: FileImage, color: C.green },
  jpeg: { icon: FileImage, color: C.green },
  gif: { icon: FileImage, color: C.green },
  svg: { icon: FileImage, color: C.orange },
  webp: { icon: FileImage, color: C.green },
  ico: { icon: FileImage, color: C.green },
  bmp: { icon: FileImage, color: C.green },

  // Video
  mp4: { icon: FileVideo, color: C.pink },
  mov: { icon: FileVideo, color: C.pink },
  avi: { icon: FileVideo, color: C.pink },
  mkv: { icon: FileVideo, color: C.pink },
  webm: { icon: FileVideo, color: C.pink },

  // Audio
  mp3: { icon: FileAudio, color: C.purple },
  wav: { icon: FileAudio, color: C.purple },
  ogg: { icon: FileAudio, color: C.purple },
  flac: { icon: FileAudio, color: C.purple },

  // Spreadsheet / Tabular
  csv: { icon: FileSpreadsheet, color: C.green },
  tsv: { icon: FileSpreadsheet, color: C.green },
  parquet: { icon: FileSpreadsheet, color: C.cyan },
  arrow: { icon: FileSpreadsheet, color: C.cyan },

  // Database
  db: { icon: Database, color: C.cyan },
  sqlite: { icon: Database, color: C.cyan },
  sqlite3: { icon: Database, color: C.cyan },
  sql: { icon: Database, color: C.yellow },

  // Archives
  zip: { icon: FileArchive, color: C.orange },
  tar: { icon: FileArchive, color: C.orange },
  gz: { icon: FileArchive, color: C.orange },
  tgz: { icon: FileArchive, color: C.orange },
  "7z": { icon: FileArchive, color: C.orange },
  rar: { icon: FileArchive, color: C.orange },
  dmg: { icon: FileArchive, color: C.orange },

  // ML / Data Science
  safetensors: { icon: Binary, color: C.purple },
  onnx: { icon: Binary, color: C.cyan },
  npy: { icon: Binary, color: C.green },
  npz: { icon: Binary, color: C.green },
  h5: { icon: Binary, color: C.orange },
  hdf5: { icon: Binary, color: C.orange },
  pt: { icon: Binary, color: C.red },
  pth: { icon: Binary, color: C.red },
  pkl: { icon: Binary, color: C.yellow },

  // PDF
  pdf: { icon: FileText, color: C.red },

  // Lock files
  lock: { icon: FileText, color: C.slate },

  // Docker
  dockerfile: { icon: FileCode, color: C.cyan },

  // GraphQL / Protobuf
  graphql: { icon: FileCode, color: C.pink },
  gql: { icon: FileCode, color: C.pink },
  proto: { icon: FileCode, color: C.cyan },
};

/**
 * Special filename matches (case-insensitive) for files without a
 * meaningful extension or with well-known names.
 */
const NAME_ICON: Record<string, IconDef> = {
  dockerfile: { icon: FileCode, color: C.cyan },
  makefile: { icon: FileCode, color: C.orange },
  ".gitignore": { icon: Settings, color: C.slate },
  ".gitattributes": { icon: Settings, color: C.slate },
  ".env": { icon: Shield, color: C.yellow },
  ".env.local": { icon: Shield, color: C.yellow },
  ".env.production": { icon: Shield, color: C.yellow },
  "cargo.toml": { icon: Settings, color: C.orange },
  "cargo.lock": { icon: FileText, color: C.slate },
  "package.json": { icon: FileJson, color: C.green },
  "tsconfig.json": { icon: FileJson, color: C.blue },
};

export function FileIcon({ path, kind, size = 13 }: { path: string; kind: EntryKind; size?: number }) {
  if (kind === "pointer" || kind === "lfs") {
    return <Binary size={size} className="shrink-0 text-primary" />;
  }

  // Try matching by full filename first (e.g. Dockerfile, .gitignore).
  const filename = path.split("/").pop()?.toLowerCase() ?? "";
  const byName = NAME_ICON[filename];
  if (byName) {
    const Icon = byName.icon;
    return <Icon size={size} className={`shrink-0 ${byName.color}`} />;
  }

  // Fall back to extension-based lookup.
  const dot = path.lastIndexOf(".");
  const ext = dot === -1 ? "" : path.slice(dot + 1).toLowerCase();
  const byExt = EXT_ICON[ext];
  if (byExt) {
    const Icon = byExt.icon;
    return <Icon size={size} className={`shrink-0 ${byExt.color}`} />;
  }

  // Default: generic file icon.
  return <FileIconLucide size={size} className="shrink-0 text-muted-foreground" />;
}
