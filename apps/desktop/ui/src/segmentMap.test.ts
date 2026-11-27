import { describe, expect, it } from "vitest";
import { SEGMENT_MAP_BINS } from "@vortex/design-tokens";
import { generateDemoSegmentMap, summarizeSegmentBins } from "./segmentMap.js";

describe("512-bin Canvas segment map state", () => {
  it("produces exactly 512 bins with active worker heads in range 2..33", () => {
    const bins = generateDemoSegmentMap(12);
    expect(bins.length).toBe(SEGMENT_MAP_BINS);

    const summary = summarizeSegmentBins(bins);
    expect(summary.active).toBe(12);
    expect(summary.completed + summary.active + summary.remaining).toBe(512);
    for (const val of bins) {
      expect(val).toBeGreaterThanOrEqual(0);
      expect(val).toBeLessThanOrEqual(33);
    }
  });
});
