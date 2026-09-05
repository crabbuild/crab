import { describe, expect, it } from "vitest"

import { GET } from "./route"

describe("Crab pointer specification", () => {
  it("publishes the canonical v1 header and legacy read contract", async () => {
    const response = GET()
    const body = await response.text()

    expect(response.status).toBe(200)
    expect(response.headers.get("content-type")).toBe(
      "text/plain; charset=utf-8"
    )
    expect(body).toContain("version https://crab.build/spec/v1")
    expect(body).toContain("version https://crab.dev/spec/v1")
  })
})
