import { describe, expect, it } from "vitest"

import { formatBlogDate } from "@/lib/blog-date"

describe("formatBlogDate", () => {
  it("preserves the calendar date for date-only ISO values", () => {
    expect(formatBlogDate("2026-05-01")).toBe("May 1, 2026")
  })

  it("supports the compact month used by blog index cards", () => {
    expect(formatBlogDate("2026-08-14", "short")).toBe("Aug 14, 2026")
  })
})
