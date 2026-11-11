const escapes: Record<string, number> = { a: 7, b: 8, t: 9, n: 10, v: 11, f: 12, r: 13, '"': 34, "\\": 92 };
const encoder = new TextEncoder();
const decoder = new TextDecoder("utf-8", { fatal: true });

/** Decodes Git's C-quoted UTF-8 paths, rejecting malformed or unrepresentable names. */
export function decodeGitPath(value: string): string | null {
  if (value.includes("\0")) return null;
  if (!value.startsWith('"')) return value;
  if (!value.endsWith('"') || value.length < 2) return null;
  const inner = value.slice(1, -1);
  const bytes: number[] = [];
  for (let index = 0; index < inner.length;) {
    if (inner[index] !== "\\") {
      const next = inner.indexOf("\\", index);
      const end = next < 0 ? inner.length : next;
      if (inner.slice(index, end).includes('"')) return null;
      bytes.push(...encoder.encode(inner.slice(index, end)));
      index = end;
      continue;
    }
    const escape = inner[index + 1];
    const byte = escapes[escape];
    if (byte !== undefined) { bytes.push(byte); index += 2; continue; }
    const octal = inner.slice(index + 1, index + 4);
    if (!/^[0-3][0-7]{2}$/.test(octal)) return null;
    bytes.push(parseInt(octal, 8));
    index += 4;
  }
  if (bytes.includes(0)) return null;
  // Git octal escapes encode bytes. Decoding each escape as a Unicode
  // character breaks UTF-8 file identity in diffs and mutation anchors.
  try { return decoder.decode(Uint8Array.from(bytes)); } catch { return null; }
}
