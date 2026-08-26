import fs from "node:fs"
import path from "node:path"

export function countWordsFromMdxFile(
  collection: "blog" | "library",
  slug: string
): number {
  const filePath = path.join(
    process.cwd(),
    "content",
    collection,
    `${slug}.mdx`
  )

  try {
    const content = fs.readFileSync(filePath, "utf-8")
    const withoutFrontmatter = content.replace(/^---[\s\S]*?---/, "")
    const withoutCode = withoutFrontmatter.replace(/```[\s\S]*?```/g, "")

    return withoutCode.split(/\s+/).filter((word) => word.length > 0).length
  } catch {
    return 200
  }
}

export function calculateReadingTime(wordCount: number): number {
  if (wordCount <= 0) return 1
  return Math.max(1, Math.ceil(wordCount / 200))
}
