export interface ChangelogRelease {
  version: string
  date: string
  githubUrl?: string
  sourceNote?: string
  changes: {
    added?: string[]
    changed?: string[]
    fixed?: string[]
    removed?: string[]
  }
}

/**
 * Sorts changelog entries in reverse chronological order (newest first).
 */
export function sortChangelog(entries: ChangelogRelease[]): ChangelogRelease[] {
  return [...entries].sort(
    (a, b) => new Date(b.date).getTime() - new Date(a.date).getTime()
  )
}

/**
 * Formats an ISO 8601 date string as "MMMM D, YYYY" (e.g. "June 15, 2025").
 */
export function formatChangelogDate(isoDate: string): string {
  const date = new Date(isoDate)
  const month = date.toLocaleString("en-US", {
    month: "long",
    timeZone: "UTC",
  })
  const day = date.getUTCDate()
  const year = date.getUTCFullYear()
  return `${month} ${day}, ${year}`
}

/**
 * Public changelog data for Crab releases.
 *
 * Keep entries tied to published release notes, source tags, or the repository
 * changelog. Do not add aspirational roadmap items here.
 */
export const changelogData: ChangelogRelease[] = [
  {
    version: "1.0.6",
    date: "2026-07-06",
    githubUrl: "https://github.com/crabbuild/crab-release/releases/tag/v1.0.6",
    sourceNote:
      "Verified from the public v1.0.6 release notes and source tag v1.0.6 at commit 3e947d15.",
    changes: {
      changed: [
        "Improved `crab optimize` and `crab mount` workflows.",
        "The release notes list the source tag, source commit, six CLI archives, and checksum file.",
      ],
      fixed: [
        "Stabilized release gates before publishing v1.0.6.",
        "Updated release target directory coverage in the release test path.",
      ],
    },
  },
  {
    version: "1.0.5",
    date: "2026-07-04",
    githubUrl: "https://github.com/crabbuild/crab-release/releases/tag/v1.0.5",
    sourceNote:
      "Verified from the public v1.0.5 release notes and local tag v1.0.5 at commit 95454849.",
    changes: {
      added: [
        "Expanded Git LFS compatibility coverage for repositories that still use LFS pointers.",
      ],
      changed: [
        "Updated `crab mount` in the release range.",
        "Added release publishing guidance for Crab CLI artifacts.",
      ],
      fixed: ["Made the release script honor the configured target directory."],
    },
  },
  {
    version: "1.0.4",
    date: "2026-07-03",
    githubUrl: "https://github.com/crabbuild/crab-release/releases/tag/v1.0.4",
    sourceNote:
      "Verified from the public v1.0.4 release page and local tag v1.0.4 at commit ad0c0808.",
    changes: {
      changed: [
        "Published Crab CLI release artifacts for macOS, Linux, and Windows on ARM64 and x86_64.",
        "Updated Linux ARM release builds to install OpenSSL headers.",
      ],
    },
  },
]
