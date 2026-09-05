const SPEC = `Crab pointer specification v1

A canonical v1 pointer is UTF-8 text with a trailing newline:

version https://crab.build/spec/v1
file-hash <64 lowercase hexadecimal BLAKE3 digest of the complete file>
size <unsigned decimal byte length of the complete file>

An optional fourth line may provide a shard lookup hint:

shard-hint <64 lowercase hexadecimal shard digest>

Readers must also accept "version https://crab.dev/spec/v1", which was emitted by earlier Crab releases before the specification moved to crab.build.
`

export function GET() {
  return new Response(SPEC, {
    headers: {
      "Cache-Control": "public, max-age=3600, s-maxage=86400",
      "Content-Type": "text/plain; charset=utf-8",
      "X-Content-Type-Options": "nosniff",
    },
  })
}
