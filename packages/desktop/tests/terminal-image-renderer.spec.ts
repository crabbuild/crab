import { describe, it, expect } from "vitest";
import {
  calculateScaledDimensions,
  createImageAddonOptions,
  isValidImageFormat,
  DEFAULT_IMAGE_CONFIG,
  type ImageDimensions,
} from "../src/lib/terminal-image-renderer";

describe("terminal-image-renderer", () => {
  describe("calculateScaledDimensions", () => {
    it("returns original dimensions when image fits within viewport", () => {
      const result = calculateScaledDimensions({ width: 400, height: 300 }, 800);
      expect(result.width).toBe(400);
      expect(result.height).toBe(300);
      expect(result.scaled).toBe(false);
    });

    it("scales proportionally when image exceeds viewport width", () => {
      const result = calculateScaledDimensions({ width: 1600, height: 900 }, 800);
      expect(result.width).toBe(800);
      expect(result.height).toBeCloseTo(450);
      expect(result.scaled).toBe(true);
    });

    it("handles exact viewport width (no scaling needed)", () => {
      const result = calculateScaledDimensions({ width: 800, height: 600 }, 800);
      expect(result.width).toBe(800);
      expect(result.height).toBe(600);
      expect(result.scaled).toBe(false);
    });

    it("maintains aspect ratio when scaling", () => {
      const image: ImageDimensions = { width: 2000, height: 1000 };
      const viewport = 500;
      const result = calculateScaledDimensions(image, viewport);
      const originalRatio = image.width / image.height;
      const scaledRatio = result.width / result.height;
      expect(scaledRatio).toBeCloseTo(originalRatio, 5);
    });

    it("handles very small images without scaling", () => {
      const result = calculateScaledDimensions({ width: 16, height: 16 }, 800);
      expect(result.width).toBe(16);
      expect(result.height).toBe(16);
      expect(result.scaled).toBe(false);
    });

    it("handles zero-width image gracefully", () => {
      const result = calculateScaledDimensions({ width: 0, height: 100 }, 800);
      expect(result.width).toBe(0);
      expect(result.height).toBe(100);
      expect(result.scaled).toBe(false);
    });

    it("handles zero viewport width gracefully", () => {
      const result = calculateScaledDimensions({ width: 400, height: 300 }, 0);
      expect(result.width).toBe(400);
      expect(result.height).toBe(300);
      expect(result.scaled).toBe(false);
    });

    it("scales a wide image to exactly viewport width", () => {
      const result = calculateScaledDimensions({ width: 3000, height: 2000 }, 600);
      expect(result.width).toBe(600);
      // 2000 * (600/3000) = 400
      expect(result.height).toBeCloseTo(400);
    });
  });

  describe("createImageAddonOptions", () => {
    it("uses viewport width when maxWidthPx is 0", () => {
      const options = createImageAddonOptions(DEFAULT_IMAGE_CONFIG, 1024);
      expect(options.pixelLimit).toBe(1024 * 4096);
    });

    it("uses configured maxWidthPx when set", () => {
      const config = { ...DEFAULT_IMAGE_CONFIG, maxWidthPx: 512 };
      const options = createImageAddonOptions(config, 1024);
      expect(options.pixelLimit).toBe(512 * 4096);
    });

    it("enables sixel support when enabled", () => {
      const options = createImageAddonOptions(DEFAULT_IMAGE_CONFIG, 800);
      expect(options.sixelSupport).toBe(true);
      expect(options.iip).toBe(true);
    });

    it("disables protocols when disabled", () => {
      const config = { ...DEFAULT_IMAGE_CONFIG, enabled: false };
      const options = createImageAddonOptions(config, 800);
      expect(options.sixelSupport).toBe(false);
      expect(options.iip).toBe(false);
    });

    it("configures scrollback retention", () => {
      const options = createImageAddonOptions(DEFAULT_IMAGE_CONFIG, 800);
      expect(options.sixelScrolling).toBe(true);
    });
  });

  describe("isValidImageFormat", () => {
    it("detects PNG format from base64 data", () => {
      // PNG magic bytes: 89 50 4E 47 0D 0A 1A 0A
      const pngBytes = new Uint8Array([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0]);
      const base64 = btoa(String.fromCharCode(...pngBytes));
      const result = isValidImageFormat(base64);
      expect(result.valid).toBe(true);
      expect(result.format).toBe("png");
    });

    it("detects JPEG format from base64 data", () => {
      // JPEG magic bytes: FF D8 FF
      const jpegBytes = new Uint8Array([0xff, 0xd8, 0xff, 0xe0, 0, 0, 0, 0, 0, 0, 0, 0]);
      const base64 = btoa(String.fromCharCode(...jpegBytes));
      const result = isValidImageFormat(base64);
      expect(result.valid).toBe(true);
      expect(result.format).toBe("jpeg");
    });

    it("detects GIF format from base64 data", () => {
      // GIF magic bytes: 47 49 46 38
      const gifBytes = new Uint8Array([0x47, 0x49, 0x46, 0x38, 0x39, 0x61, 0, 0, 0, 0, 0, 0]);
      const base64 = btoa(String.fromCharCode(...gifBytes));
      const result = isValidImageFormat(base64);
      expect(result.valid).toBe(true);
      expect(result.format).toBe("gif");
    });

    it("rejects unknown format", () => {
      const unknownBytes = new Uint8Array([0x00, 0x01, 0x02, 0x03, 0, 0, 0, 0, 0, 0, 0, 0]);
      const base64 = btoa(String.fromCharCode(...unknownBytes));
      const result = isValidImageFormat(base64);
      expect(result.valid).toBe(false);
      expect(result.format).toBeNull();
    });

    it("rejects malformed base64", () => {
      const result = isValidImageFormat("not-valid-base64!!!");
      expect(result.valid).toBe(false);
      expect(result.format).toBeNull();
    });

    it("respects supported formats list", () => {
      // PNG magic bytes
      const pngBytes = new Uint8Array([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0]);
      const base64 = btoa(String.fromCharCode(...pngBytes));
      // Only allow jpeg
      const result = isValidImageFormat(base64, ["jpeg"]);
      expect(result.valid).toBe(false);
      expect(result.format).toBe("png");
    });
  });
});
