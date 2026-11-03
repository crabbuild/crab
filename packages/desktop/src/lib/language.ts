// Map a filename to a Monaco language identifier.
//
// Monaco ships with a broad set of built-in languages. This map covers
// common programming languages, config formats, and data science files.
// Unknown extensions fall back to "plaintext", which still gives you
// line numbers, search, and selection.
//
// Resolution order:
// 1. Exact basename match (BASENAME_MAP)
// 2. Glob/prefix pattern match (PATTERN_RULES — checked in order)
// 3. Extension match (EXT_MAP)
// 4. Fall back to "plaintext"

const EXT_MAP: Record<string, string> = {
  // JavaScript / TypeScript
  ts: "typescript",
  tsx: "tsx",
  mts: "typescript",
  cts: "typescript",
  js: "javascript",
  jsx: "jsx",
  mjs: "javascript",
  cjs: "javascript",

  // Web
  html: "html",
  htm: "html",
  xhtml: "html",
  css: "css",
  scss: "scss",
  less: "less",
  vue: "vue",
  svelte: "svelte",

  // Data formats
  json: "json",
  jsonc: "jsonc",
  json5: "jsonc",
  geojson: "json",
  ipynb: "json",
  yaml: "yaml",
  yml: "yaml",
  toml: "toml",
  ini: "ini",
  cfg: "ini",
  conf: "ini",
  properties: "ini",
  env: "dotenv",
  xml: "xml",
  svg: "xml",
  xsl: "xml",
  xsd: "xml",
  plist: "xml",
  csproj: "xml",
  fsproj: "xml",
  sln: "xml",
  vcxproj: "xml",
  pom: "xml",

  // Systems programming
  rs: "rust",
  c: "c",
  h: "c",
  cpp: "cpp",
  cc: "cpp",
  cxx: "cpp",
  hpp: "cpp",
  hxx: "cpp",
  hh: "cpp",
  ino: "cpp",
  go: "go",
  zig: "zig",

  // JVM
  java: "java",
  kt: "kotlin",
  kts: "kotlin",
  scala: "scala",
  groovy: "groovy",
  gradle: "groovy",

  // .NET
  cs: "csharp",
  fs: "fsharp",
  fsx: "fsharp",
  vb: "vb",

  // Scripting
  py: "python",
  pyi: "python",
  pyx: "python",
  rb: "ruby",
  rake: "ruby",
  gemspec: "ruby",
  pl: "perl",
  pm: "perl",
  lua: "lua",
  php: "php",
  r: "r",
  R: "r",
  jl: "julia",
  ex: "elixir",
  exs: "elixir",
  erl: "erlang",
  hrl: "erlang",
  clj: "clojure",
  cljs: "clojure",
  cljc: "clojure",
  dart: "dart",
  swift: "swift",
  m: "objective-c",
  mm: "objective-c",

  // Shell
  sh: "bash",
  bash: "bash",
  zsh: "bash",
  fish: "fish",
  ps1: "powershell",
  psm1: "powershell",
  psd1: "powershell",
  bat: "bat",
  cmd: "bat",

  // Database / query
  sql: "sql",
  mysql: "sql",
  pgsql: "sql",
  graphql: "graphql",
  gql: "graphql",

  // Markup / docs
  md: "markdown",
  markdown: "markdown",
  mdx: "markdown",
  rst: "restructuredtext",
  tex: "latex",
  latex: "latex",
  bib: "bibtex",

  // Config / infra
  dockerfile: "dockerfile",
  tf: "hcl",
  tfvars: "hcl",
  hcl: "hcl",
  nomad: "hcl",
  proto: "protobuf",
  lock: "ini",
  prisma: "prisma",
  nix: "nix",

  // Bazel / Starlark
  bzl: "python",
  bazel: "python",
  star: "python",
  sky: "python",

  // Make
  mk: "makefile",
  mak: "makefile",

  // CMake
  cmake: "cmake",

  // Misc
  diff: "diff",
  patch: "diff",
  asm: "asm",
  s: "asm",
  wasm: "wasm",
  wat: "wasm",
  vim: "plaintext",
  el: "plaintext",

  // Systemd unit files (INI-like)
  service: "ini",
  timer: "ini",
  socket: "ini",
  target: "ini",
  mount: "ini",

  // Gentoo / Arch
  ebuild: "bash",
  eclass: "bash",
};

// Exact basename → language. Checked first (case-sensitive).
const BASENAME_MAP: Record<string, string> = {
  // Container
  Dockerfile: "dockerfile",
  Containerfile: "dockerfile",
  Earthfile: "dockerfile",

  // Make / task runners
  Makefile: "makefile",
  makefile: "makefile",
  GNUmakefile: "makefile",
  BSDmakefile: "makefile",
  Justfile: "makefile",
  justfile: "makefile",

  // CMake
  "CMakeLists.txt": "cmake",
  "CMakeCache.txt": "cmake",

  // Bazel / Buck / Pants (Starlark → python grammar)
  BUILD: "python",
  "BUILD.bazel": "python",
  WORKSPACE: "python",
  "WORKSPACE.bazel": "python",
  "MODULE.bazel": "python",
  BUCK: "python",
  TARGETS: "python",
  "BUILD.pants": "python",
  Tiltfile: "python",

  // Meson
  "meson.build": "python",
  "meson_options.txt": "python",
  "meson.options": "python",

  // Ruby
  Rakefile: "ruby",
  Gemfile: "ruby",
  "Gemfile.lock": "ruby",
  Vagrantfile: "ruby",
  Capfile: "ruby",
  Guardfile: "ruby",
  Brewfile: "ruby",
  Berksfile: "ruby",
  Fastfile: "ruby",
  Appfile: "ruby",
  Matchfile: "ruby",
  Podfile: "ruby",
  Thorfile: "ruby",
  "config.ru": "ruby",

  // Groovy / CI
  Jenkinsfile: "groovy",

  // TOML configs
  "Cargo.toml": "toml",
  "Cargo.lock": "toml",
  "pyproject.toml": "toml",
  Pipfile: "toml",
  "poetry.lock": "toml",
  "Gopkg.toml": "toml",
  "Gopkg.lock": "toml",
  "rustfmt.toml": "toml",
  ".rustfmt.toml": "toml",
  "clippy.toml": "toml",
  ".clippy.toml": "toml",
  "rust-toolchain.toml": "toml",
  "netlify.toml": "toml",
  "wrangler.toml": "toml",
  "fly.toml": "toml",
  "pixi.toml": "toml",
  "uv.toml": "toml",

  // JSON configs
  "package.json": "json",
  "package-lock.json": "json",
  "tsconfig.json": "jsonc",
  "jsconfig.json": "jsonc",
  "deno.json": "jsonc",
  "deno.jsonc": "jsonc",
  "composer.json": "json",
  "composer.lock": "json",
  "Pipfile.lock": "json",
  "bower.json": "json",
  "lerna.json": "json",
  "nx.json": "json",
  "turbo.json": "jsonc",
  "rush.json": "json",
  "renovate.json": "json",
  "renovate.json5": "jsonc",
  "bun.lock": "jsonc",
  ".eslintrc": "jsonc",
  ".eslintrc.json": "jsonc",
  ".prettierrc": "jsonc",
  ".prettierrc.json": "jsonc",
  ".babelrc": "jsonc",
  ".babelrc.json": "jsonc",
  ".stylelintrc": "jsonc",
  ".stylelintrc.json": "jsonc",
  ".swcrc": "jsonc",
  ".tern-project": "json",
  "mcp.json": "jsonc",

  // YAML configs (exact names that lack .yml/.yaml extension)
  ".clang-format": "yaml",
  ".clang-tidy": "yaml",
  ".gemrc": "yaml",
  ".yamllint": "yaml",

  // INI / properties
  ".editorconfig": "ini",
  "setup.cfg": "ini",
  ".flake8": "ini",
  ".pylintrc": "ini",
  pylintrc: "ini",
  "tox.ini": "ini",
  ".coveragerc": "ini",
  "mypy.ini": "ini",
  ".isort.cfg": "ini",
  ".npmrc": "ini",
  ".yarnrc": "ini",
  ".curlrc": "ini",
  ".wgetrc": "ini",
  ".gitconfig": "ini",
  ".hgrc": "ini",
  "mercurial.ini": "ini",
  ".terraformrc": "hcl",
  "terraform.rc": "hcl",

  // Shell dotfiles
  ".bashrc": "bash",
  ".bash_profile": "bash",
  ".bash_login": "bash",
  ".bash_logout": "bash",
  ".bash_aliases": "bash",
  ".zshrc": "bash",
  ".zshenv": "bash",
  ".zprofile": "bash",
  ".zlogin": "bash",
  ".zlogout": "bash",
  ".zsh_aliases": "bash",
  ".profile": "bash",
  ".shrc": "bash",
  ".kshrc": "bash",
  ".cshrc": "bash",
  ".tcshrc": "bash",
  ".envrc": "bash",
  PKGBUILD: "bash",
  APKBUILD: "bash",
  ".tmux.conf": "bash",
  "tmux.conf": "bash",

  // Dotenv
  ".env": "dotenv",
  ".env.local": "dotenv",
  ".env.development": "dotenv",
  ".env.production": "dotenv",
  ".env.test": "dotenv",
  ".env.staging": "dotenv",
  ".env.example": "dotenv",
  ".flaskenv": "dotenv",

  // Nix
  "flake.nix": "nix",
  "default.nix": "nix",
  "shell.nix": "nix",
  "configuration.nix": "nix",
  "home.nix": "nix",

  // Go
  "go.mod": "go",
  "go.sum": "plaintext",
  "go.work": "go",

  // Python
  ".pythonrc": "python",
  ".pdbrc": "python",

  // Lua
  ".luacheckrc": "lua",
  ".luarc.json": "json",

  // Ignore files (comment-based list format)
  ".gitignore": "gitignore",
  ".gitattributes": "gitignore",
  ".dockerignore": "gitignore",
  ".npmignore": "gitignore",
  ".eslintignore": "gitignore",
  ".prettierignore": "gitignore",
  ".stylelintignore": "gitignore",
  ".terraformignore": "gitignore",
  ".helmignore": "gitignore",
  ".cfignore": "gitignore",
  ".gcloudignore": "gitignore",
  ".gitmodules": "ini",

  // Version pinning files (single-line or list)
  ".nvmrc": "plaintext",
  ".node-version": "plaintext",
  ".tool-versions": "plaintext",
  ".python-version": "plaintext",
  ".ruby-version": "plaintext",
  ".terraform-version": "plaintext",
  "rust-toolchain": "plaintext",

  // Docs / legal (no extension)
  LICENSE: "plaintext",
  COPYING: "plaintext",
  "COPYING.LESSER": "plaintext",
  NOTICE: "plaintext",
  AUTHORS: "plaintext",
  CONTRIBUTORS: "plaintext",
  MAINTAINERS: "plaintext",
  PATENTS: "plaintext",
  CODEOWNERS: "plaintext",

  // Procfile
  Procfile: "bash",
};

// Pattern-based rules checked in order after BASENAME_MAP misses.
// Each entry: [test function, language].
// We use functions instead of RegExp for clarity and to avoid re-compiling.
interface PatternRule {
  test: (basename: string) => boolean;
  lang: string;
}

const PATTERN_RULES: PatternRule[] = [
  // Dockerfile variants: Dockerfile.*, *.Dockerfile, *.dockerfile
  { test: (n) => n.startsWith("Dockerfile.") || n.startsWith("dockerfile."), lang: "dockerfile" },
  { test: (n) => n.endsWith(".Dockerfile") || n.endsWith(".dockerfile"), lang: "dockerfile" },

  // Makefile variants: Makefile.*
  { test: (n) => n.startsWith("Makefile.") || n.startsWith("makefile."), lang: "makefile" },

  // Jenkinsfile variants: Jenkinsfile.*
  { test: (n) => n.startsWith("Jenkinsfile."), lang: "groovy" },

  // .env variants: .env.* (e.g. .env.staging, .env.test.local)
  { test: (n) => n.startsWith(".env."), lang: "dotenv" },

  // tsconfig / jsconfig variants: tsconfig.*.json, jsconfig.*.json
  { test: (n) => /^[jt]sconfig\..*\.json$/.test(n), lang: "jsonc" },

  // Docker compose variants
  { test: (n) => /^(docker-)?compose.*\.ya?ml$/.test(n), lang: "yaml" },

  // RC files that are typically JSON/JSONC (.*rc without known extension)
  { test: (n) => /^\.\w+rc\.json$/.test(n), lang: "jsonc" },
  { test: (n) => /^\.\w+rc\.ya?ml$/.test(n), lang: "yaml" },
  { test: (n) => /^\.\w+rc\.js$/.test(n), lang: "javascript" },
  { test: (n) => /^\.\w+rc\.ts$/.test(n), lang: "typescript" },
];

export function languageForPath(path: string): string {
  const slash = path.lastIndexOf("/");
  const name = slash === -1 ? path : path.slice(slash + 1);

  // 1. Exact basename match
  const exact = BASENAME_MAP[name];
  if (exact) return exact;

  // 2. Pattern rules (order matters — first match wins)
  for (const rule of PATTERN_RULES) {
    if (rule.test(name)) return rule.lang;
  }

  // 3. Extension match
  const dot = name.lastIndexOf(".");
  if (dot !== -1) {
    const ext = name.slice(dot + 1).toLowerCase();
    const mapped = EXT_MAP[ext];
    if (mapped) return mapped;
  }

  return "plaintext";
}
