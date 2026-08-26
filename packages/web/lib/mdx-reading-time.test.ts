import { describe, expect, it } from "vitest"

import { calculateReadingTime } from "@/lib/mdx-reading-time"

describe("calculateReadingTime", () => {
  it("returns one minute for empty content", () => {
    expect(calculateReadingTime(0)).toBe(1)
  })

  it("rounds partial minutes up", () => {
    expect(calculateReadingTime(201)).toBe(2)
  })
})
