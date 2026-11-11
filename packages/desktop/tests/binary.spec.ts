import { describe, expect, it } from "vitest";
import { decodeText } from "../src/lib/binary";

describe("editable text decoding", () => {
  it("preserves Unicode and empty text", () => {
    for (const text of ["", "Hello, 蟹 🦀\n", "a\tline\r\n"]) {
      expect(decodeText(new TextEncoder().encode(text))).toBe(text);
    }
  });

  it("rejects invalid UTF-8 beyond the control-byte sample", () => {
    const bytes = new Uint8Array(16 * 1024).fill(0x61);
    bytes[bytes.length - 1] = 0xff;
    expect(decodeText(bytes)).toBeNull();
  });

  it("keeps NUL-containing data out of the text editor", () => {
    expect(decodeText(new Uint8Array([0x61, 0, 0x62]))).toBeNull();
  });
});
