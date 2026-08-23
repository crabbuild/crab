import { readFile, readdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

const docsRoot = new URL("../content/docs/cli/", import.meta.url);
const strict = process.argv.includes("--strict");

const minimumWords = {
  Conceptual: 300,
  "How-to": 300,
  Landing: 250,
  Reference: 120,
  Troubleshooting: 300,
  Tutorial: 280,
};

const visualTypes = new Set([
  "Conceptual",
  "How-to",
  "Landing",
  "Troubleshooting",
  "Tutorial",
]);

async function collectMdx(directory) {
  const entries = await readdir(directory, { withFileTypes: true });
  const files = [];

  for (const entry of entries) {
    const target = new URL(`${entry.name}${entry.isDirectory() ? "/" : ""}`, directory);
    if (entry.isDirectory()) {
      files.push(...(await collectMdx(target)));
    } else if (entry.name.endsWith(".mdx")) {
      files.push(target);
    }
  }

  return files;
}

function frontmatter(source) {
  if (!source.startsWith("---\n")) return { body: source, data: {} };
  const end = source.indexOf("\n---\n", 4);
  if (end === -1) return { body: source, data: {} };

  const raw = source.slice(4, end);
  const data = {};

  for (const line of raw.split("\n")) {
    const nested = line.match(/^  ([A-Za-z]+):\s*["']?(.*?)["']?$/);
    if (nested && data.meta) {
      data.meta[nested[1]] = nested[2];
      continue;
    }

    const field = line.match(/^([A-Za-z]+):\s*["']?(.*?)["']?$/);
    if (!field) continue;
    if (field[1] === "meta" && field[2] === "") {
      data.meta = {};
    } else {
      data[field[1]] = field[2];
    }
  }

  return { body: source.slice(end + 5), data };
}

function analyzeCodeFences(body) {
  const lines = body.split("\n");
  let open = false;
  let tagged = true;

  for (const line of lines) {
    const fence = line.match(/^```(.*)$/);
    if (!fence) continue;
    if (!open && fence[1].trim() === "") tagged = false;
    open = !open;
  }

  return {
    hasCode: lines.some((line) => /^```\S+/.test(line)),
    tagged,
    balanced: !open,
  };
}

function prose(body) {
  return body
    .replace(/```[\s\S]*?```/g, " ")
    .replace(/<[^>]+>/g, " ")
    .replace(/^---[\s\S]*?---$/gm, " ")
    .replace(/!\[[^\]]*\]\([^)]*\)/g, " ")
    .replace(/\[[^\]]+\]\([^)]*\)/g, " ")
    .replace(/[`*_#>|{}:[\]()-]/g, " ")
    .replace(/\s+/g, " ")
    .trim();
}

function sectionFor(relativePath) {
  const parts = relativePath.split(path.sep);
  return parts.length === 1 ? "root" : parts[0];
}

const files = await collectMdx(docsRoot);
const pages = [];

for (const file of files) {
  const source = await readFile(file, "utf8");
  const relativePath = path.relative(fileURLToPath(docsRoot), fileURLToPath(file));
  const { body, data } = frontmatter(source);
  const code = analyzeCodeFences(body);
  const text = prose(body);
  const words = text === "" ? 0 : text.split(/\s+/).length;
  const contentType = data.meta?.contentType;
  const hasDiagram = /<DocsPathDiagram\b|```mermaid\b|!\[[^\]]*\]\([^)]*\.(?:svg|png|webp|jpg|jpeg)\)/.test(
    body,
  );
  const hasTable = /^\|.+\|$/m.test(body) && /^\|\s*:?-+/m.test(body);
  const hasExample = /(^|\n)#{2,4}\s+.*(?:example|workflow|try|verify|use|run|inspect)/i.test(body) || code.hasCode;
  const wordingSource = body.replaceAll(/Glacier Instant Retrieval/gi, "provider storage class");
  const risky = [...wordingSource.matchAll(/\b(?:easy|easily|simple|simply|quick|quickly|very|just|really|instant|instantly)\b/gi)].map(
    (match) => match[0].toLowerCase(),
  );
  const findings = [];

  if (!data.title) findings.push("missing title");
  if (!data.description) findings.push("missing description");
  if (!contentType) findings.push("missing content type");
  if (!data.meta?.goal) findings.push("missing goal");
  if (!data.meta?.audience) findings.push("missing audience");
  if (contentType && words < minimumWords[contentType]) {
    findings.push(`${words}/${minimumWords[contentType]} words`);
  }
  if (contentType && visualTypes.has(contentType) && !hasDiagram && !hasTable) {
    findings.push("missing visual evidence");
  }
  if (contentType !== "Landing" && !hasExample) findings.push("missing example");
  if (!code.tagged) findings.push("untagged code fence");
  if (!code.balanced) findings.push("unbalanced code fence");
  if (risky.length > 0) findings.push(`risky wording: ${[...new Set(risky)].join(", ")}`);

  pages.push({
    relativePath,
    section: sectionFor(relativePath),
    contentType: contentType ?? "Unclassified",
    words,
    hasDiagram,
    hasTable,
    hasCode: code.hasCode,
    findings,
  });
}

pages.sort((a, b) => a.relativePath.localeCompare(b.relativePath));
const findings = pages.filter((page) => page.findings.length > 0);
const sections = Map.groupBy(pages, (page) => page.section);

console.log(`Docs content audit: ${pages.length} pages`);
console.log(`Pages with findings: ${findings.length}`);
console.log("");
console.log("Section                       Pages  Median words  Findings");

for (const [section, sectionPages] of [...sections].sort(([a], [b]) => a.localeCompare(b))) {
  const sortedWords = sectionPages.map((page) => page.words).sort((a, b) => a - b);
  const median = sortedWords[Math.floor(sortedWords.length / 2)];
  const findingCount = sectionPages.filter((page) => page.findings.length > 0).length;
  console.log(`${section.padEnd(29)} ${String(sectionPages.length).padStart(5)} ${String(median).padStart(13)} ${String(findingCount).padStart(9)}`);
}

if (findings.length > 0) {
  console.log("\nPage findings:");
  for (const page of findings) {
    console.log(`- ${page.relativePath}: ${page.findings.join("; ")}`);
  }
}

if (strict && findings.length > 0) process.exitCode = 1;
