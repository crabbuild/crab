import { mkdir, readFile, rm, writeFile } from "node:fs/promises"
import { createRequire } from "node:module"
import path from "node:path"
import { fileURLToPath } from "node:url"

import { renderToStaticMarkup } from "react-dom/server"
import sharp from "sharp"
import ts from "typescript"

const scriptDirectory = path.dirname(fileURLToPath(import.meta.url))
const webDirectory = path.resolve(scriptDirectory, "..")
const coreSceneSource = path.join(
  webDirectory,
  "components/blog/launch-motion-scenes.tsx"
)
const storageSceneSource = path.join(
  webDirectory,
  "components/blog/launch-motion-storage-scenes.tsx"
)
const temporaryCoreModule = path.join(
  scriptDirectory,
  ".launch-motion-scenes.cjs"
)
const temporaryStorageModule = path.join(
  scriptDirectory,
  ".launch-motion-storage-scenes.cjs"
)
const outputDirectory = path.join(webDirectory, "public/animations")
const width = 720
const height = 336
const holdMilliseconds = 720
const transitionMilliseconds = 90
const transitionFrames = 4

const exports = [
  [
    "DedupMotionScene",
    "crab-chunk-reuse",
    "Content-defined chunk reuse",
    "Stable chunk identities are reused while only new bytes enter a new xorb.",
  ],
  [
    "PublishMotionScene",
    "crab-durable-publish",
    "Durable before visible publication",
    "The Git and Crab closures become durable before the main ref advances.",
  ],
  [
    "HydrateMotionScene",
    "crab-selective-hydration",
    "Selective lazy hydration",
    "Only required xorb ranges are read, reconstructed, and verified.",
  ],
  [
    "GcMotionScene",
    "crab-reachability-gc",
    "Reachability-safe garbage collection",
    "Reachable data is retained, recent orphans are protected, and only old unreachable objects are eligible.",
  ],
]

async function compileSceneModule(sourceFile) {
  const source = await readFile(sourceFile, "utf8")
  return ts.transpileModule(source, {
    compilerOptions: {
      target: ts.ScriptTarget.ES2022,
      module: ts.ModuleKind.CommonJS,
      jsx: ts.JsxEmit.ReactJSX,
      esModuleInterop: true,
    },
    fileName: sourceFile,
  }).outputText
}

async function loadScenes() {
  const coreModule = await compileSceneModule(coreSceneSource)
  const storageModule = (await compileSceneModule(storageSceneSource)).replace(
    'require("@/components/blog/launch-motion-scenes")',
    'require("./.launch-motion-scenes.cjs")'
  )

  await writeFile(temporaryCoreModule, coreModule)
  await writeFile(temporaryStorageModule, storageModule)
  const require = createRequire(import.meta.url)
  return {
    ...require(temporaryCoreModule),
    ...require(temporaryStorageModule),
  }
}

async function renderPhase(Scene, phase, id) {
  const markup = renderToStaticMarkup(Scene({ phase, id, animate: false }))
  return sharp(Buffer.from(markup))
    .resize(width, height, { fit: "fill" })
    .ensureAlpha()
    .raw()
    .toBuffer()
}

function escapeXml(value) {
  return value
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
}

function svgContents(markup) {
  const match = markup.match(/^<svg[^>]*>([\s\S]*)<\/svg>$/)
  if (!match)
    throw new Error("Rendered motion scene did not contain an SVG root")
  return match[1]
    .replace(/<title>[\s\S]*?<\/title>/, "")
    .replace(/<desc>[\s\S]*?<\/desc>/, "")
}

async function exportSvg(Scene, basename, title, description) {
  const durationSeconds = (holdMilliseconds * 5) / 1_000
  const phaseMarkup = Array.from({ length: 5 }, (_, phase) => {
    const markup = renderToStaticMarkup(
      Scene({
        phase,
        id: `${basename}-phase-${phase}`,
        animate: true,
      })
    )
    return `<g class="crab-export-phase crab-export-phase-${phase}">${svgContents(markup)}</g>`
  }).join("\n")
  const phaseRules = Array.from(
    { length: 5 },
    (_, phase) =>
      `.crab-export-phase-${phase}{animation-delay:${phase * (durationSeconds / 5)}s}`
  ).join("\n")
  const svg = `<?xml version="1.0" encoding="UTF-8"?>
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 900 420" width="900" height="420" role="img" aria-labelledby="title description">
  <title id="title">${escapeXml(title)}</title>
  <desc id="description">${escapeXml(description)}</desc>
  <style>
    .crab-export-phase{opacity:0;animation:crab-export-phase ${durationSeconds}s linear infinite}
    ${phaseRules}
    .crab-export-phase-0{opacity:1}
    @keyframes crab-export-phase{0%{opacity:0}4%,20%{opacity:1}24%,100%{opacity:0}}
    @media (prefers-reduced-motion:reduce){.crab-export-phase{animation:none;opacity:0}.crab-export-phase-4{opacity:1}}
  </style>
  ${phaseMarkup}
</svg>
`
  await writeFile(path.join(outputDirectory, `${basename}.svg`), svg)
}

function blendFrames(from, to, amount) {
  const blended = Buffer.allocUnsafe(from.length)
  for (let index = 0; index < from.length; index += 1) {
    blended[index] = Math.round(from[index] * (1 - amount) + to[index] * amount)
  }
  return blended
}

async function pngFrame(raw) {
  return sharp(raw, { raw: { width, height, channels: 4 } })
    .png()
    .toBuffer()
}

async function exportGif(Scene, basename) {
  const phaseFrames = await Promise.all(
    Array.from({ length: 5 }, (_, phase) => renderPhase(Scene, phase, basename))
  )
  const frames = []
  const delays = []

  for (let phase = 0; phase < phaseFrames.length; phase += 1) {
    const current = phaseFrames[phase]
    const next = phaseFrames[(phase + 1) % phaseFrames.length]
    frames.push(await pngFrame(current))
    delays.push(holdMilliseconds)

    for (let step = 1; step <= transitionFrames; step += 1) {
      frames.push(
        await pngFrame(
          blendFrames(current, next, step / (transitionFrames + 1))
        )
      )
      delays.push(transitionMilliseconds)
    }
  }

  await sharp(frames, { join: { animated: true } })
    .gif({
      loop: 0,
      delay: delays,
      colours: 128,
      dither: 0.55,
      effort: 7,
      interFrameMaxError: 5,
    })
    .toFile(path.join(outputDirectory, `${basename}.gif`))
}

await mkdir(outputDirectory, { recursive: true })
const scenes = await loadScenes()

try {
  for (const [exportName, basename, title, description] of exports) {
    await exportSvg(scenes[exportName], basename, title, description)
    await exportGif(scenes[exportName], basename)
    process.stdout.write(`exported public/animations/${basename}.{svg,gif}\n`)
  }
} finally {
  await rm(temporaryCoreModule, { force: true })
  await rm(temporaryStorageModule, { force: true })
}
