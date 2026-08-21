import { describe, it, expect } from 'vitest'
import * as fc from 'fast-check'

describe('test infrastructure', () => {
  it('vitest runs', () => {
    expect(true).toBe(true)
  })

  it('fast-check works', () => {
    fc.assert(
      fc.property(fc.integer(), (n) => {
        return n + 0 === n
      }),
    )
  })
})
