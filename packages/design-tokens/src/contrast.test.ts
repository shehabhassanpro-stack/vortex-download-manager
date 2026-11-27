import { describe, expect, it } from "vitest";
import { DARK_PALETTE, LIGHT_PALETTE, contrastRatio, type ThemePalette } from "./index.js";

function verifyWcagAa(name: string, palette: ThemePalette): void {
  const surfaces = [palette.bgCanvas, palette.bgSurface];

  for (const bg of surfaces) {
    expect(
      contrastRatio(palette.textPrimary, bg),
      `${name}: textPrimary on ${bg}`,
    ).toBeGreaterThanOrEqual(10);

    expect(
      contrastRatio(palette.textSecondary, bg),
      `${name}: textSecondary on ${bg}`,
    ).toBeGreaterThanOrEqual(5.5);

    expect(
      contrastRatio(palette.textMuted, bg),
      `${name}: textMuted on ${bg}`,
    ).toBeGreaterThanOrEqual(4.5);

    // Status indicators and brand accent on surfaces must meet WCAG AA text/UI contrast (>= 4.5:1)
    expect(
      contrastRatio(palette.statusActive, bg),
      `${name}: statusActive on ${bg}`,
    ).toBeGreaterThanOrEqual(4.5);
    expect(
      contrastRatio(palette.statusSuccess, bg),
      `${name}: statusSuccess on ${bg}`,
    ).toBeGreaterThanOrEqual(4.5);
    expect(
      contrastRatio(palette.statusWarning, bg),
      `${name}: statusWarning on ${bg}`,
    ).toBeGreaterThanOrEqual(4.5);
    expect(
      contrastRatio(palette.statusDanger, bg),
      `${name}: statusDanger on ${bg}`,
    ).toBeGreaterThanOrEqual(4.5);
    expect(
      contrastRatio(palette.statusQueued, bg),
      `${name}: statusQueued on ${bg}`,
    ).toBeGreaterThanOrEqual(4.5);
  }

  // Primary CTA label on brand background must meet WCAG AA (>= 4.5:1)
  expect(
    contrastRatio(palette.brandOnPrimary, palette.brandPrimary),
    `${name}: brandOnPrimary on brandPrimary`,
  ).toBeGreaterThanOrEqual(4.5);
}

describe("@vortex/design-tokens WCAG 2.2 AA & Anti-Halation invariants", () => {
  it("avoids pure black #000000 in Dark Mode (anti-halation OLED rule)", () => {
    expect(DARK_PALETTE.bgCanvas.toLowerCase()).not.toBe("#000000");
    expect(DARK_PALETTE.bgSurface.toLowerCase()).not.toBe("#000000");
  });

  it("satisfies WCAG 2.2 AA contrast ratios across all Light theme tokens", () => {
    verifyWcagAa("Light", LIGHT_PALETTE);
  });

  it("satisfies WCAG 2.2 AA contrast ratios across all Dark theme tokens", () => {
    verifyWcagAa("Dark", DARK_PALETTE);
  });
});
