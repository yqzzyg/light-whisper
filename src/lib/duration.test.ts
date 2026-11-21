import { describe, expect, it } from "vitest";

import { formatDuration, formatLatencySeconds } from "@/lib/duration";

describe("formatLatencySeconds", () => {
  it("renders latencies in seconds with one decimal", () => {
    expect(formatLatencySeconds(1000)).toBe("1.0");
    expect(formatLatencySeconds(1900)).toBe("1.9");
    expect(formatLatencySeconds(80_892)).toBe("80.9");
    expect(formatLatencySeconds(317_700)).toBe("317.7");
  });

  it("keeps two decimals below the tenth-of-a-second threshold", () => {
    expect(formatLatencySeconds(100)).toBe("0.1");
    expect(formatLatencySeconds(420)).toBe("0.4");
    expect(formatLatencySeconds(42)).toBe("0.04");
    expect(formatLatencySeconds(0)).toBe("0.00");
  });

  it("returns null for missing or invalid values", () => {
    expect(formatLatencySeconds(null)).toBeNull();
    expect(formatLatencySeconds(undefined)).toBeNull();
    expect(formatLatencySeconds(Number.NaN)).toBeNull();
    expect(formatLatencySeconds(Number.POSITIVE_INFINITY)).toBeNull();
    expect(formatLatencySeconds(-1)).toBeNull();
  });
});

describe("formatDuration", () => {
  it("pads minutes and seconds under an hour", () => {
    expect(formatDuration(0)).toBe("00:00");
    expect(formatDuration(7)).toBe("00:07");
    expect(formatDuration(65)).toBe("01:05");
    expect(formatDuration(599)).toBe("09:59");
  });

  it("switches to hours once the recording passes one hour", () => {
    expect(formatDuration(3600)).toBe("1:00:00");
    expect(formatDuration(3725)).toBe("1:02:05");
    expect(formatDuration(36_000)).toBe("10:00:00");
  });

  it("treats negative, missing and non-finite values as zero", () => {
    expect(formatDuration(-5)).toBe("00:00");
    expect(formatDuration(null)).toBe("00:00");
    expect(formatDuration(undefined)).toBe("00:00");
    expect(formatDuration(Number.NaN)).toBe("00:00");
    expect(formatDuration(Number.POSITIVE_INFINITY)).toBe("00:00");
  });

  it("floors fractional seconds so the timer never shows a value ahead of the file", () => {
    expect(formatDuration(1.9)).toBe("00:01");
  });
});
