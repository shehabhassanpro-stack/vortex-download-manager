import { SEGMENT_MAP_BINS } from "@vortex/design-tokens";

/**
 * Synthesizes a deterministic 512-bin segment map matching the Core IPC format
 * (`Design-System.md` §4.2): `0` = remaining, `1` = completed, `2..33` = active worker head.
 */
export function generateDemoSegmentMap(workerCount = 8): Uint8Array {
  const bins = new Uint8Array(SEGMENT_MAP_BINS);
  const clampedWorkers = Math.max(1, Math.min(32, workerCount));
  const slice = Math.floor(SEGMENT_MAP_BINS / clampedWorkers);

  for (let w = 0; w < clampedWorkers; w += 1) {
    const start = w * slice;
    const end = w === clampedWorkers - 1 ? SEGMENT_MAP_BINS : (w + 1) * slice;
    const progress = 0.52 + ((w * 13) % 35) / 100;
    const head = Math.min(end - 1, start + Math.floor((end - start) * progress));

    for (let i = start; i < head; i += 1) {
      bins[i] = 1;
    }
    bins[head] = 2 + w;
  }

  return bins;
}

/** Counts completed, active, and remaining bins for summary display and tests. */
export function summarizeSegmentBins(bins: Uint8Array): {
  readonly completed: number;
  readonly active: number;
  readonly remaining: number;
} {
  let completed = 0;
  let active = 0;
  let remaining = 0;
  for (const val of bins) {
    if (val === 1) {
      completed += 1;
    } else if (val >= 2) {
      active += 1;
    } else {
      remaining += 1;
    }
  }
  return { completed, active, remaining };
}
