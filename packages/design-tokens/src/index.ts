/**
 * Vortex OKLCH Calm Design Tokens & WCAG 2.2 AA Contrast Utilities (`P0-T17`).
 */

export interface ThemePalette {
  readonly brandPrimary: string;
  readonly brandHover: string;
  readonly brandOnPrimary: string;
  readonly bgCanvas: string;
  readonly bgSurface: string;
  readonly bgElevated: string;
  readonly textPrimary: string;
  readonly textSecondary: string;
  readonly textMuted: string;
  readonly statusActive: string;
  readonly statusSuccess: string;
  readonly statusWarning: string;
  readonly statusDanger: string;
  readonly statusQueued: string;
}

export const LIGHT_PALETTE: ThemePalette = {
  brandPrimary: "#4F46E5",
  brandHover: "#4338CA",
  brandOnPrimary: "#FFFFFF",
  bgCanvas: "#F5F6F8",
  bgSurface: "#FFFFFF",
  bgElevated: "#F9FAFB",
  textPrimary: "#111827",
  textSecondary: "#475569",
  textMuted: "#5E6D82",
  statusActive: "#2563EB",
  statusSuccess: "#15803D",
  statusWarning: "#B45309",
  statusDanger: "#C81E1E",
  statusQueued: "#7C3AED",
};

export const DARK_PALETTE: ThemePalette = {
  brandPrimary: "#7C7BFF",
  brandHover: "#9190FF",
  brandOnPrimary: "#0F1117",
  bgCanvas: "#13151A",
  bgSurface: "#1B1E26",
  bgElevated: "#232732",
  textPrimary: "#ECEFF4",
  textSecondary: "#A3ACB9",
  textMuted: "#8B95A5",
  statusActive: "#60A5FA",
  statusSuccess: "#4ADE80",
  statusWarning: "#FBBF24",
  statusDanger: "#F87171",
  statusQueued: "#C084FC",
};

export const SPACING_PX = {
  s1: 4,
  s2: 8,
  s3: 12,
  s4: 16,
  s5: 20,
  s6: 24,
  s8: 32,
} as const;

export const RADIUS_PX = {
  control: 6,
  card: 8,
  modal: 12,
  pill: 9999,
} as const;

export const MOTION_MS = {
  micro: 120,
  ui: 180,
  modal: 220,
} as const;

export const DOWNLOAD_ROW_HEIGHT_PX = 48 as const;
export const SEGMENT_MAP_BINS = 512 as const;

function parseHexRgb(hex: string): readonly [number, number, number] {
  const clean = hex.trim().replace(/^#/, "");
  if (clean.length !== 6) {
    throw new Error(`Expected 6-digit hex color, got: ${hex}`);
  }
  const r = Number.parseInt(clean.slice(0, 2), 16);
  const g = Number.parseInt(clean.slice(2, 4), 16);
  const b = Number.parseInt(clean.slice(4, 6), 16);
  return [r, g, b] as const;
}

function channelLuminance(channel8: number): number {
  const srgb = channel8 / 255;
  return srgb <= 0.04045 ? srgb / 12.92 : ((srgb + 0.055) / 1.055) ** 2.4;
}

/** Computes WCAG 2.2 relative luminance in `[0, 1]`. */
export function relativeLuminance(hex: string): number {
  const [r, g, b] = parseHexRgb(hex);
  return 0.2126 * channelLuminance(r) + 0.7152 * channelLuminance(g) + 0.0722 * channelLuminance(b);
}

/** Computes WCAG 2.2 contrast ratio in `[1, 21]`. */
export function contrastRatio(foregroundHex: string, backgroundHex: string): number {
  const l1 = relativeLuminance(foregroundHex);
  const l2 = relativeLuminance(backgroundHex);
  const lighter = Math.max(l1, l2);
  const darker = Math.min(l1, l2);
  return (lighter + 0.05) / (darker + 0.05);
}
