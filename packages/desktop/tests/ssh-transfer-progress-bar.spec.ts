import { describe, it, expect } from "vitest";
import { formatSpeed, formatEta, formatBytes } from "../src/components/ssh/TransferProgressBar";

describe("TransferProgressBar helpers", () => {
  describe("formatSpeed", () => {
    it("formats zero as 0.0 B/s", () => {
      expect(formatSpeed(0)).toBe("0.0 B/s");
    });

    it("formats negative values as 0.0 B/s", () => {
      expect(formatSpeed(-100)).toBe("0.0 B/s");
    });

    it("formats bytes per second below 1024", () => {
      expect(formatSpeed(500)).toBe("500.0 B/s");
    });

    it("auto-scales to KB/s", () => {
      expect(formatSpeed(1024)).toBe("1.0 KB/s");
      expect(formatSpeed(1536)).toBe("1.5 KB/s");
    });

    it("auto-scales to MB/s", () => {
      expect(formatSpeed(1048576)).toBe("1.0 MB/s");
      expect(formatSpeed(2621440)).toBe("2.5 MB/s");
    });

    it("auto-scales to GB/s", () => {
      expect(formatSpeed(1073741824)).toBe("1.0 GB/s");
    });

    it("uses one decimal place", () => {
      expect(formatSpeed(1536)).toBe("1.5 KB/s");
      expect(formatSpeed(2560)).toBe("2.5 KB/s");
    });
  });

  describe("formatEta", () => {
    it("returns dash for zero seconds", () => {
      expect(formatEta(0)).toBe("—");
    });

    it("returns dash for negative seconds", () => {
      expect(formatEta(-10)).toBe("—");
    });

    it("returns dash for Infinity", () => {
      expect(formatEta(Infinity)).toBe("—");
    });

    it("formats seconds under a minute", () => {
      expect(formatEta(45)).toBe("45s");
    });

    it("formats minutes and seconds", () => {
      expect(formatEta(150)).toBe("2m 30s");
    });

    it("formats exact minutes without trailing seconds", () => {
      expect(formatEta(120)).toBe("2m");
    });

    it("formats hours and minutes", () => {
      expect(formatEta(3661)).toBe("1h 1m");
    });

    it("formats exact hours without trailing minutes", () => {
      expect(formatEta(3600)).toBe("1h");
    });
  });

  describe("formatBytes", () => {
    it("formats zero bytes", () => {
      expect(formatBytes(0)).toBe("0 B");
    });

    it("formats negative as 0 B", () => {
      expect(formatBytes(-100)).toBe("0 B");
    });

    it("formats bytes below 1024", () => {
      expect(formatBytes(512)).toBe("512 B");
    });

    it("auto-scales to KB", () => {
      expect(formatBytes(1024)).toBe("1.0 KB");
    });

    it("auto-scales to MB", () => {
      expect(formatBytes(47400000)).toBe("45.2 MB");
    });

    it("auto-scales to GB", () => {
      expect(formatBytes(1073741824)).toBe("1.0 GB");
    });

    it("uses one decimal place for scaled units", () => {
      expect(formatBytes(1536)).toBe("1.5 KB");
    });
  });
});
