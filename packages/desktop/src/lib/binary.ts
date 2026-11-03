// Heuristic: decide whether a blob is text or binary.
//
// VS Code uses a similar approach — sample the first ~8 KB and look
// for NUL bytes or a high ratio of non-printable characters. The
// heuristic is intentionally conservative: false-positives ("binary
// when it's actually text") just fall back to hex view, while
// false-negatives would let Monaco choke on invalid UTF-8 and crash.

const SAMPLE_BYTES = 8 * 1024;
const NONPRINTABLE_THRESHOLD = 0.3;

export function isProbablyBinary(bytes: Uint8Array): boolean {
  const n = Math.min(bytes.length, SAMPLE_BYTES);
  if (n === 0) return false;
  let nonPrintable = 0;
  for (let i = 0; i < n; i++) {
    const b = bytes[i];
    // NUL byte → almost certainly binary.
    if (b === 0) return true;
    // Printable ASCII range + common whitespace (\t, \n, \r, \f).
    const printable =
      (b >= 0x20 && b < 0x7f) || b === 0x09 || b === 0x0a || b === 0x0d || b === 0x0c || b >= 0x80;
    if (!printable) nonPrintable++;
  }
  return nonPrintable / n > NONPRINTABLE_THRESHOLD;
}

// Decode base64 to Uint8Array in a browser-safe way. atob + manual
// copy is the widely-supported path; Node's Buffer is not available
// in the sandbox'd renderer.
export function base64ToBytes(b64: string): Uint8Array {
  const bin = atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

// Decode bytes as UTF-8; if the bytes are invalid UTF-8 (non-BMP
// garbage inside a claimed text file), TextDecoder with fatal:false
// still produces a lossy string — the caller should have ruled that
// out with `isProbablyBinary` first.
export function bytesToUtf8(bytes: Uint8Array): string {
  return new TextDecoder("utf-8").decode(bytes);
}

// Render first N bytes as a classic hex dump.
// Format:
//   00000000: 48 65 6c 6c 6f  Hello
// 16 bytes per row, grouped in pairs. Matches `xxd -g 1` closely
// enough that most devs recognise it at a glance.
export function hexDump(bytes: Uint8Array, maxBytes = 4096): string {
  const n = Math.min(bytes.length, maxBytes);
  const lines: string[] = [];
  for (let off = 0; off < n; off += 16) {
    const chunk = bytes.subarray(off, Math.min(off + 16, n));
    const hex: string[] = [];
    const ascii: string[] = [];
    for (let i = 0; i < 16; i++) {
      if (i < chunk.length) {
        hex.push(chunk[i].toString(16).padStart(2, "0"));
        const b = chunk[i];
        ascii.push(b >= 0x20 && b < 0x7f ? String.fromCharCode(b) : ".");
      } else {
        hex.push("  ");
        ascii.push(" ");
      }
    }
    lines.push(
      `${off.toString(16).padStart(8, "0")}  ${hex.slice(0, 8).join(" ")}  ${hex.slice(8).join(" ")}  ${ascii.join("")}`,
    );
  }
  if (n < bytes.length) {
    lines.push(`… ${bytes.length - n} more bytes not shown`);
  }
  return lines.join("\n");
}
