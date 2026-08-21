import { spawn } from "node:child_process"
import process from "node:process"

const host = "127.0.0.1"
const port = process.env.CRAB_WEB_LINK_CHECK_PORT ?? "3210"
const origin = `http://${host}:${port}`
const next = process.platform === "win32" ? "next.cmd" : "next"
const server = spawn(next, ["start", "--hostname", host, "--port", port], {
  cwd: process.cwd(),
  env: process.env,
  stdio: ["ignore", "pipe", "pipe"],
})
let serverOutput = ""
for (const stream of [server.stdout, server.stderr]) {
  stream.setEncoding("utf8")
  stream.on("data", (chunk) => {
    serverOutput += chunk
  })
}

const pages = new Map()
const queued = new Set(["/"])
const queue = ["/"]
const fragments = []
const failures = []

try {
  await waitForServer()
  for (let cursor = 0; cursor < queue.length; cursor += 12) {
    const batch = queue.slice(cursor, cursor + 12)
    await Promise.all(batch.map(checkPage))
  }
  for (const { source, target, fragment } of fragments) {
    const ids = pages.get(target)
    if (ids && !ids.has(fragment)) {
      failures.push(`${source} links to missing fragment ${target}#${fragment}`)
    }
  }
  if (failures.length > 0) {
    failures.sort()
    throw new Error(`documentation link check failed:\n${failures.join("\n")}`)
  }
  console.log(
    `ok: checked ${pages.size} HTML pages and ${fragments.length} fragments`
  )
} finally {
  server.kill("SIGTERM")
  await Promise.race([
    new Promise((resolve) => server.once("exit", resolve)),
    new Promise((resolve) => setTimeout(resolve, 5_000)),
  ])
}

async function waitForServer() {
  for (let attempt = 0; attempt < 60; attempt += 1) {
    if (server.exitCode !== null) {
      throw new Error(`Next.js server exited before startup:\n${serverOutput}`)
    }
    try {
      const response = await fetch(origin, {
        signal: AbortSignal.timeout(1_000),
      })
      if (response.ok) return
    } catch {
      // The production server may not have bound its socket yet.
    }
    await new Promise((resolve) => setTimeout(resolve, 500))
  }
  throw new Error(
    `Next.js server did not start within 30 seconds:\n${serverOutput}`
  )
}

async function checkPage(path) {
  let response
  try {
    response = await fetch(`${origin}${path}`, {
      redirect: "follow",
      signal: AbortSignal.timeout(10_000),
    })
  } catch (error) {
    failures.push(`${path} could not be fetched: ${error.message}`)
    return
  }
  if (!response.ok) {
    failures.push(`${path} returned HTTP ${response.status}`)
    return
  }
  const contentType = response.headers.get("content-type") ?? ""
  if (!contentType.startsWith("text/html")) return

  const html = await response.text()
  pages.set(path, attributes(html, "id"))
  for (const href of attributes(html, "href")) {
    const decoded = decodeAttribute(href)
    if (/^(?:data|javascript|mailto|tel):/i.test(decoded)) continue
    let target
    try {
      target = new URL(decoded, `${origin}${path}`)
    } catch {
      failures.push(`${path} contains an invalid href: ${decoded}`)
      continue
    }
    if (target.origin !== origin) continue
    const targetPath = `${target.pathname}${target.search}`
    if (!queued.has(targetPath)) {
      queued.add(targetPath)
      queue.push(targetPath)
    }
    if (target.hash.length > 1) {
      fragments.push({
        source: path,
        target: targetPath,
        fragment: decodeURIComponent(target.hash.slice(1)),
      })
    }
  }
}

function attributes(html, name) {
  const values = new Set()
  const expression = new RegExp(`\\b${name}=(?:"([^"]*)"|'([^']*)')`, "gi")
  for (const match of html.matchAll(expression)) {
    values.add(match[1] ?? match[2])
  }
  return values
}

function decodeAttribute(value) {
  return value
    .replaceAll("&amp;", "&")
    .replaceAll("&quot;", '"')
    .replaceAll("&#39;", "'")
    .replaceAll("&#x27;", "'")
}
