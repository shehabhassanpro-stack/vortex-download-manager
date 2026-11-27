import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import axe from "axe-core";
import { Linter } from "eslint";
import { JSDOM } from "jsdom";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import eslintConfig from "../../../../eslint.config.mjs";
import { App } from "./App.js";
import type { StagedDownloadDto } from "./bindings/client.js";
import { SmartConfirmModal } from "./features/capture/SmartConfirmModal.js";
import { DiagnosticsPanel } from "./features/diagnostics/DiagnosticsPanel.js";
import { SettingsModal } from "./features/settings/SettingsModal.js";

const currentDir = path.dirname(fileURLToPath(import.meta.url));

async function assertZeroAxeViolations(markup: string, wrapInMain = false): Promise<void> {
  const bodyContent = wrapInMain ? `<main>${markup}</main>` : markup;
  const dom = new JSDOM(
    `<!doctype html><html lang="ar" dir="rtl" data-theme="dark"><head><title>Vortex</title></head><body>${bodyContent}</body></html>`,
    { pretendToBeVisual: true },
  );

  const results = await axe.run(dom.window.document.documentElement, {
    rules: {
      // Color contrast is verified mathematically in @vortex/design-tokens contrast.test.ts
      "color-contrast": { enabled: false },
    },
  });

  expect(results.violations).toEqual([]);
}

const SAMPLE_DANGER_STAGED: StagedDownloadDto = {
  staged_id: "0192f3a4-0000-7000-8000-000000000888",
  url: "https://sketchy-cdn.example/video.mp4",
  display_url: "https://sketchy-cdn.example/video.mp4",
  suggested_file_name: "Movie.2026.mp4.exe",
  category_id: "programs",
  suggested_save_dir: "D:/Downloads/Programs",
  total_size: 4_194_304,
  supports_range: true,
  free_disk_bytes: 200_000_000_000,
  security: {
    risk_level: "danger",
    warnings: ["executable_masquerade", "high_risk_executable", "cross_origin_ad_redirect"],
    detected_signature: "PE_EXE",
    block_auto_start: true,
    block_enter_shortcut: true,
    origin_host: "sketchy-cdn.example",
    referrer_host: "free-movies.example",
    cross_origin_redirected: true,
  },
  duplicate: null,
  source: "determining-filename",
  burst_quarantined: true,
  created_at_ms: 1_764_000_000_000,
  expires_at_ms: 1_764_000_120_000,
};

describe("WCAG 2.2 AA (axe-core) & RTL CSS Logical Properties guard", () => {
  it("passes axe-core WCAG 2.2 AA audit with 0 violations on Main Edge-to-Edge UI", async () => {
    const markup = renderToStaticMarkup(createElement(App));
    await assertZeroAxeViolations(markup);
  });

  it("passes axe-core WCAG 2.2 AA audit on SmartConfirmModal (Danger Masquerade + Burst Quarantine)", async () => {
    const markup = renderToStaticMarkup(
      createElement(SmartConfirmModal, {
        stagedQueue: [SAMPLE_DANGER_STAGED, { ...SAMPLE_DANGER_STAGED, staged_id: "second-id" }],
        manualMode: false,
        standaloneWindow: true,
        onConfirmed: () => undefined,
        onDiscardStaged: async () => undefined,
        onDiscardAllStaged: async () => undefined,
        onBlockHost: async () => undefined,
        onCloseManual: () => undefined,
      }),
    );
    expect(markup).toContain("sketchy-cdn.example");
    await assertZeroAxeViolations(markup);
  });

  it("passes axe-core WCAG 2.2 AA audit on SettingsModal and DiagnosticsPanel", async () => {
    const settingsMarkup = renderToStaticMarkup(
      createElement(SettingsModal, {
        config: {
          schema_version: 1,
          max_connections_per_download: 8,
          max_concurrent_downloads: 3,
          checkpoint_interval_secs: 20,
          idle_timeout_secs: 30,
          max_retries: 10,
          min_free_disk_bytes: 268_435_456,
          default_download_dir: "D:/Downloads",
          auto_start_handoff: false,
          warm_instant_webview: false,
          prevent_sleep_while_active: true,
          clipboard_monitor_enabled: true,
          theme: "dark",
          language: "ar",
          global_speed_limit_bps: null,
          capture_rules: {
            enabled: true,
            min_file_size_bytes: 1_048_576,
            extensions: ["zip"],
            mime_types: ["application/zip"],
            excluded_hosts: ["localhost"],
            capture_in_incognito: false,
          },
          feature_flags: {
            enable_http3: false,
            enable_ledbat: false,
            enable_wasm_plugins: false,
            enable_youtube_hybrid: false,
          },
        },
        theme: "dark",
        onChangeTheme: () => undefined,
        onChangeLanguage: () => undefined,
        onSave: async () => undefined,
        onClose: () => undefined,
      }),
    );
    await assertZeroAxeViolations(settingsMarkup, true);

    const diagMarkup = renderToStaticMarkup(createElement(DiagnosticsPanel));
    await assertZeroAxeViolations(diagMarkup, true);
  });

  it("enforces zero physical left/right declarations across CSS files", () => {
    const cssFiles = [
      path.resolve(currentDir, "styles.css"),
      path.resolve(currentDir, "../../../../packages/design-tokens/tokens.css"),
      path.resolve(currentDir, "../../../../extension/src/entrypoints/popup/style.css"),
      path.resolve(currentDir, "../../../../extension/src/entrypoints/options/style.css"),
    ];

    const bannedPhysicalCss =
      /(?:margin|padding|border)-(?:left|right)\s*:|text-align\s*:\s*(?:left|right)|float\s*:\s*(?:left|right)/i;

    for (const file of cssFiles) {
      if (!fs.existsSync(file)) continue;
      const text = fs.readFileSync(file, "utf8");
      expect(
        bannedPhysicalCss.test(text),
        `Found banned physical left/right CSS property in ${file}`,
      ).toBe(false);
    }
  });

  it("ESLint vortex-rtl/no-physical-directions blocks physical classes and allows logical ones", () => {
    const linter = new Linter({ configType: "flat" });

    const badMessages = linter.verify(
      `export const Bad = () => <div className="ml-4 text-left" style={{ marginLeft: 8 }} />;`,
      eslintConfig,
      { filename: "Bad.tsx" },
    );
    expect(
      badMessages.filter((m) => m.ruleId === "vortex-rtl/no-physical-directions").length,
    ).toBeGreaterThanOrEqual(2);

    const goodMessages = linter.verify(
      `export const Good = () => <div className="ms-4 text-start" style={{ marginInlineStart: 8 }} />;`,
      eslintConfig,
      { filename: "Good.tsx" },
    );
    expect(goodMessages.filter((m) => m.ruleId === "vortex-rtl/no-physical-directions")).toEqual(
      [],
    );
  });
});
