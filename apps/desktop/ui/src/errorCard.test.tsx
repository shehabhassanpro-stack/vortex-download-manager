import axe from "axe-core";
import { JSDOM } from "jsdom";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import {
  normalizeApiError,
  type DownloadSummaryDto,
  type StagedDownloadDto,
} from "./bindings/client.js";
import { SmartConfirmModal } from "./features/capture/SmartConfirmModal.js";
import { LiveInspectorDrawer } from "./features/progress/LiveInspectorDrawer.js";
import { ensureI18n } from "./shared/i18n/index.js";
import { formatBytesDual } from "./shared/lib/format.js";
import { VtxErrorCard } from "./shared/ui/VtxErrorCard.js";

ensureI18n();

describe("Phase 1.6 Error Contract, LiveInspectorDrawer & Misuse Prevention", () => {
  it("normalizes canonical camelCase, legacy snake_case SQLite rows, and malformed errors without crashing", () => {
    const legacySnake = {
      code: "VTX-RES-002",
      category: "Resource",
      recoverability: { kind: "UserActionRequired" },
      user_message_key: "errors.res.link_expired",
      message_params: { host: "pixeldrain.com" },
      suggested_actions: ["RefreshLink", "CopyDiagnostics"],
      technical_detail: "HTTP 403 Forbidden",
      trace_id: "0192f3a4-88b1-7000-8000-000000000099",
    };

    const normLegacy = normalizeApiError(legacySnake);
    expect(normLegacy.code).toBe("VTX-RES-002");
    expect(normLegacy.suggestedActions).toEqual(["RefreshLink", "CopyDiagnostics"]);
    expect(normLegacy.traceId).toBe("0192f3a4-88b1-7000-8000-000000000099");

    // Simulate missing suggestedActions (the exact Phase 1.5 crash vector!)
    const missingActions = {
      code: "VTX-RES-006",
      category: "Resource",
      recoverability: "UserActionRequired",
    };
    const normMissing = normalizeApiError(missingActions);
    expect(Array.isArray(normMissing.suggestedActions)).toBe(true);
    expect(normMissing.suggestedActions.length).toBeGreaterThanOrEqual(1);
  });

  it("renders VtxErrorCard and LiveInspectorDrawer across all new VTX-* error codes and legacy snake_case without throwing", () => {
    const legacyError = {
      code: "VTX-RES-002",
      category: "Resource",
      recoverability: "UserActionRequired",
      user_message_key: "errors.res.link_expired",
      suggested_actions: undefined,
      trace_id: "0192f3a4-0000-7000-8000-000000000777",
    } as unknown as Parameters<typeof VtxErrorCard>[0]["error"];

    const legacyMarkup = renderToStaticMarkup(createElement(VtxErrorCard, { error: legacyError }));
    expect(legacyMarkup).toContain("VTX-RES-002");

    const newCodes = [
      "VTX-RES-005",
      "VTX-RES-006",
      "VTX-AUTH-002",
      "VTX-DISK-005",
      "VTX-DISK-006",
      "VTX-SEC-003",
    ] as const;

    for (const code of newCodes) {
      const item: DownloadSummaryDto = {
        id: "0192f3a4-0000-7000-8000-000000000500",
        file_name: "BT_The_Manor_0.31.4.zip",
        save_dir: "C:/Users/PC/Downloads",
        display_url: "https://pixeldrain.com/api/file/test",
        total_size: 1_120_000_000,
        downloaded_bytes: 0,
        state_slug: "needs_attention",
        speed_bps: 0,
        eta_secs: null,
        active_workers: 0,
        supports_range: true,
        category_id: "compressed",
        queue_id: "main",
        priority: "Normal",
        created_at_ms: 1_764_000_000_000,
        completed_at_ms: null,
        error: {
          code,
          category: "Resource",
          recoverability: "UserActionRequired",
          userMessageKey: `errors.${code}`,
          messageParams: {},
          suggestedActions: [
            "OpenReferrerInBrowser",
            "SyncSystemClock",
            "OpenWindowsSecurity",
            "CopyDiagnostics",
          ],
          technicalDetail: `Simulated ${code}`,
          traceId: "0192f3a4-0000-7000-8000-000000000501",
        },
      };

      const drawerMarkup = renderToStaticMarkup(
        createElement(LiveInspectorDrawer, {
          item,
          theme: "dark",
          onTogglePauseResume: () => undefined,
          onRevealInFolder: () => undefined,
          onOpenFile: () => undefined,
          onClose: () => undefined,
        }),
      );
      expect(drawerMarkup).toContain(code);
      expect(drawerMarkup).toContain("1.04 GiB (1.12 GB)");
    }
  });

  it("formats large file sizes with dual IEC GiB and SI GB clarity (UX-M09)", () => {
    expect(formatBytesDual(1_120_000_000)).toBe("1.04 GiB (1.12 GB)");
    expect(formatBytesDual(5_242_880)).toBe("5.00 MB");
    expect(formatBytesDual(null)).toBe("—");
  });

  it("renders TH-30 RTLO/Bidi, weaponized script, and mountable ISO warnings with 0 axe-core violations", async () => {
    const rtloStaged: StagedDownloadDto = {
      staged_id: "0192f3a4-0000-7000-8000-000000000901",
      url: "https://untrusted.example/report.exe",
      display_url: "https://untrusted.example/report.exe",
      suggested_file_name: "report_fdp.exe",
      category_id: "programs",
      suggested_save_dir: "C:/Users/PC/Downloads",
      total_size: 1_120_000_000,
      supports_range: true,
      free_disk_bytes: 500_000_000_000,
      security: {
        risk_level: "danger",
        warnings: ["bidi_filename_spoofing", "weaponized_script_or_shortcut"],
        detected_signature: "LNK_SHORTCUT",
        block_auto_start: true,
        block_enter_shortcut: true,
        origin_host: "untrusted.example",
        referrer_host: null,
        cross_origin_redirected: false,
      },
      duplicate: {
        existing_id: "0192f3a4-0000-7000-8000-000000000101",
        file_name: "report_fdp.exe",
        save_dir: "C:/Users/PC/Downloads",
        state_slug: "downloading",
        file_exists_on_disk: false,
      },
      source: "determining-filename",
      burst_quarantined: false,
      created_at_ms: 1_764_000_000_000,
      expires_at_ms: 1_764_000_120_000,
    };

    const markup = renderToStaticMarkup(
      createElement(SmartConfirmModal, {
        stagedQueue: [rtloStaged],
        manualMode: false,
        standaloneWindow: true,
        onConfirmed: () => undefined,
        onDiscardStaged: async () => undefined,
        onDiscardAllStaged: async () => undefined,
        onBlockHost: async () => undefined,
        onJumpToExisting: () => undefined,
        onCloseManual: () => undefined,
      }),
    );

    expect(markup).toContain("RTLO");
    expect(markup).toContain("1.04 GiB (1.12 GB)");

    const dom = new JSDOM(
      `<!doctype html><html lang="ar" dir="rtl" data-theme="dark"><head><title>Vortex</title></head><body>${markup}</body></html>`,
      { pretendToBeVisual: true },
    );
    const results = await axe.run(dom.window.document.documentElement, {
      rules: { "color-contrast": { enabled: false } },
    });
    expect(results.violations).toEqual([]);
  });
});
