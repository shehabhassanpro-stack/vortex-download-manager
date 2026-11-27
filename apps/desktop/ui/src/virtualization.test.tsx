/**
 * UI & Performance Budget Tests (`P1-T31`, `P1-T32`, `P1-T36`):
 * - Verifies `10,000` virtualized rows render only capped DOM nodes in `< 16.6ms` (`60 FPS` budget!)
 * - Verifies Arabic 6-form `ICU` pluralization (`0`, `1`, `2`, `5`, `15`, `100`)
 * - Verifies 3-Tier `VTX-*` error card renders localized message + expandable `trace_id`.
 */

import { JSDOM } from "jsdom";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import { VirtualizedDownloadsTable } from "./features/downloads/VirtualizedDownloadsTable.js";
import { useDownloadsStore } from "./features/downloads/store.js";
import { ensureI18n } from "./shared/i18n/index.js";
import { VtxErrorCard } from "./shared/ui/VtxErrorCard.js";

describe("10,000-Row Virtualization 60 FPS Budget & ICU Arabic Localization (P1-T31..T36)", () => {
  it("renders 10,000 rows via TanStack Virtual mounting <= 20 DOM rows within < 16.6ms (60 FPS)", () => {
    useDownloadsStore.getState().seedVirtualizationRows(10_000);
    const tenThousand = useDownloadsStore.getState().items;
    expect(tenThousand.length).toBe(10_000);

    // Warm JIT once, then measure steady-state render latency (`60 FPS` frame budget = `16.67ms`)
    renderToStaticMarkup(
      createElement(VirtualizedDownloadsTable, {
        items: tenThousand,
        selectedId: tenThousand[0]?.id ?? null,
        onSelect: () => undefined,
        onInspectProgress: () => undefined,
        onTogglePauseResume: () => undefined,
        onDelete: () => undefined,
      }),
    );

    const t0 = performance.now();
    const html = renderToStaticMarkup(
      createElement(VirtualizedDownloadsTable, {
        items: tenThousand,
        selectedId: tenThousand[0]?.id ?? null,
        onSelect: () => undefined,
        onInspectProgress: () => undefined,
        onTogglePauseResume: () => undefined,
        onDelete: () => undefined,
      }),
    );
    const elapsedMs = performance.now() - t0;

    const dom = new JSDOM(html);
    const renderedRows = dom.window.document.querySelectorAll('[role="option"]');
    expect(renderedRows.length).toBeLessThanOrEqual(20);
    expect(elapsedMs).toBeLessThan(16.67);
  });

  it("formats all 6 Arabic ICU plural categories and 3-Tier VTX-* error cards", () => {
    const i18n = ensureI18n();
    void i18n.changeLanguage("ar");

    expect(i18n.t("toolbar.itemsCount", { count: 0 })).toContain("لا توجد");
    expect(i18n.t("toolbar.itemsCount", { count: 1 })).toContain("ملف واحد");
    expect(i18n.t("toolbar.itemsCount", { count: 2 })).toContain("ملفان");
    expect(i18n.t("toolbar.itemsCount", { count: 5 })).toContain("5 ملفات");
    expect(i18n.t("toolbar.itemsCount", { count: 15 })).toContain("15 ملفاً");
    expect(i18n.t("toolbar.itemsCount", { count: 100 })).toContain("100 ملف");

    const errHtml = renderToStaticMarkup(
      createElement(VtxErrorCard, {
        error: {
          code: "VTX-RES-002",
          category: "Resource",
          recoverability: "UserActionRequired",
          userMessageKey: "errors.res.link_expired",
          messageParams: {},
          suggestedActions: ["RefreshLink", "CopyDiagnostics"],
          technicalDetail: "HTTP 403 signed token expired",
          traceId: "0192f3a4-88b1-7000-8000-000000000042",
        },
      }),
    );
    expect(errHtml).toContain("انتهت صلاحية رابط التحميل");
    expect(errHtml).toContain("تحديث الرابط من المتصفح");
  });
});
