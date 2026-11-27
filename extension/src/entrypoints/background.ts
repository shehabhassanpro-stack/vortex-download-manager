import type { CaptureRules } from "@vortex/protocol";
import { defineBackground } from "wxt/utils/define-background";
import { CaptureDecisionEngine, DEFAULT_CAPTURE_RULES } from "../lib/capture-engine";
import { collectMergedCookies, deriveTopLevelSite } from "../lib/cookies";
import { NATIVE_HOST_ID, NativeBridgeClient, handleDeterminingFilename } from "../lib/live-handoff";
import {
  ExtensionRingBuffer,
  MAX_RING_BUFFER_EVENTS,
  generateUuidV7,
  type ExtensionLogEvent,
} from "../lib/ring-buffer";

const CONTEXT_MENU_ID = "vortex-download-with-vortex";
const SESSION_RULES_KEY = "vortexCaptureRules";
const SESSION_LOGS_KEY = "vortexRingLogs";
const ALT_BYPASS_WINDOW_MS = 3_000;

export interface ExtensionUiMessage {
  readonly kind:
    | "vortex.altKey"
    | "vortex.getStatus"
    | "vortex.updateRules"
    | "vortex.quickDownload"
    | "vortex.getLogs";
  readonly rules?: CaptureRules;
  readonly url?: string;
}

export default defineBackground(() => {
  const engine = new CaptureDecisionEngine(DEFAULT_CAPTURE_RULES);
  const ring = new ExtensionRingBuffer(MAX_RING_BUFFER_EVENTS);
  let altBypassUntilMs = 0;

  const persistSessionState = (): void => {
    void chrome.storage.session
      .set({
        [SESSION_RULES_KEY]: engine.currentRules(),
        [SESSION_LOGS_KEY]: ring.snapshot(),
      })
      .catch(() => {
        // Ignore transient session quota errors
      });
  };

  // Hydrate cached rules & ring-buffer logs across MV3 Service Worker idle/wake cycles
  void chrome.storage.session
    .get([SESSION_RULES_KEY, SESSION_LOGS_KEY])
    .then((stored) => {
      const savedRules = stored[SESSION_RULES_KEY] as CaptureRules | undefined;
      if (savedRules !== undefined) {
        engine.updateRules(savedRules);
      }
      const savedLogs = stored[SESSION_LOGS_KEY] as readonly ExtensionLogEvent[] | undefined;
      if (Array.isArray(savedLogs)) {
        for (const ev of savedLogs) {
          ring.push(ev.level, ev.traceId, ev.event, ev.detail, ev.tsMs);
        }
      }
    })
    .catch(() => {
      // Fresh session
    });

  const bridge = new NativeBridgeClient(
    {
      connect: () => chrome.runtime.connectNative(NATIVE_HOST_ID),
    },
    engine,
    ring,
    () => {
      persistSessionState();
    },
  );

  // 1. Live-Stream Hold WITHOUT `downloads.pause()` (`P1-T28`)
  chrome.downloads.onDeterminingFilename.addListener((item, suggest) => {
    const held = handleDeterminingFilename(item, suggest, {
      engine,
      bridge,
      cookiesApi: chrome.cookies,
      downloadsApi: chrome.downloads,
      ring,
      userAgent: navigator.userAgent,
      isAltBypassActive: () => Date.now() < altBypassUntilMs,
    });
    persistSessionState();
    return held;
  });

  // 2. Context Menu (`⬇ تحميل بـ Vortex`) (`P1-T29`)
  chrome.runtime.onInstalled.addListener(() => {
    chrome.contextMenus.removeAll(() => {
      chrome.contextMenus.create({
        id: CONTEXT_MENU_ID,
        title: "⬇ تحميل بـ Vortex / Download with Vortex",
        contexts: ["link", "video", "audio"],
      });
    });
  });

  chrome.contextMenus.onClicked.addListener((info, tab) => {
    if (info.menuItemId !== CONTEXT_MENU_ID) {
      return;
    }
    const targetUrl = info.linkUrl ?? info.srcUrl ?? "";
    if (!targetUrl.startsWith("http://") && !targetUrl.startsWith("https://")) {
      return;
    }
    const pageUrl = tab?.url ?? info.pageUrl ?? "";
    const traceId = generateUuidV7();
    void (async (): Promise<void> => {
      try {
        const topLevelSite = deriveTopLevelSite(pageUrl);
        const cookies = await collectMergedCookies(chrome.cookies, targetUrl, topLevelSite);
        await bridge.sendDownloadHandoff(
          {
            url: targetUrl,
            finalUrl: targetUrl,
            referrer: pageUrl,
            suggestedName: "",
            mimeType: "",
            userAgent: navigator.userAgent,
            cookies,
            headers: {},
            incognito: tab?.incognito ?? false,
            source: "context-menu",
            autoStart: false,
            topLevelSite,
          },
          traceId,
        );
        ring.push("INFO", traceId, "contextMenu.sent", targetUrl);
      } catch (err) {
        const detail = err instanceof Error ? err.message : String(err);
        ring.push("WARN", traceId, "contextMenu.fallback_browser", `${detail} ${targetUrl}`);
        // Zero-Lost-Download: start normal browser download if Core is unavailable!
        await chrome.downloads.download({ url: targetUrl });
      } finally {
        persistSessionState();
      }
    })();
  });

  // 3. Content Script (`Alt` key), Popup, and Options messaging (`P1-T29` & `TH-33`)
  chrome.runtime.onMessage.addListener((rawMsg: unknown, sender, sendResponse) => {
    // Strict MV3 sender verification (`TH-33`): reject any cross-extension or unauthenticated sender
    if (sender.id !== chrome.runtime.id) {
      return false;
    }
    if (typeof rawMsg !== "object" || rawMsg === null || !("kind" in rawMsg)) {
      return false;
    }
    const msg = rawMsg as ExtensionUiMessage;

    // Privileged settings / quick-download / log-read commands are restricted to Extension UI pages
    // (`popup.html` / `options.html`, whether popped out or opened in a browser tab) — NEVER
    // callable from untrusted webpage content scripts whose `sender.url`/`sender.origin` is `http(s)://`!
    const extensionOrigin = `chrome-extension://${chrome.runtime.id}`;
    const isExtensionUiPage =
      sender.origin === extensionOrigin ||
      (sender.url !== undefined && sender.url.startsWith(`${extensionOrigin}/`));

    if (msg.kind === "vortex.altKey") {
      altBypassUntilMs = Date.now() + ALT_BYPASS_WINDOW_MS;
      sendResponse({ ok: true });
      return false;
    }

    if (!isExtensionUiPage) {
      return false;
    }

    if (msg.kind === "vortex.getStatus") {
      const tid = generateUuidV7();
      void bridge
        .ensureHandshake(tid)
        .then(() => {
          sendResponse({
            connected: true,
            rules: engine.currentRules(),
          });
        })
        .catch(() => {
          sendResponse({
            connected: false,
            rules: engine.currentRules(),
          });
        });
      return true;
    }

    if (msg.kind === "vortex.updateRules" && msg.rules !== undefined) {
      engine.updateRules(msg.rules);
      persistSessionState();
      sendResponse({ ok: true, rules: engine.currentRules() });
      return false;
    }

    if (msg.kind === "vortex.quickDownload" && typeof msg.url === "string") {
      const targetUrl = msg.url.trim();
      const tid = generateUuidV7();
      void bridge
        .sendDownloadHandoff(
          {
            url: targetUrl,
            finalUrl: targetUrl,
            referrer: "",
            suggestedName: "",
            mimeType: "",
            userAgent: navigator.userAgent,
            cookies: "",
            headers: {},
            incognito: false,
            source: "context-menu",
            autoStart: false,
            topLevelSite: "",
          },
          tid,
        )
        .then((ack) => {
          persistSessionState();
          sendResponse({ ok: ack.accepted, ack });
        })
        .catch((err: unknown) => {
          persistSessionState();
          sendResponse({
            ok: false,
            error: err instanceof Error ? err.message : String(err),
          });
        });
      return true;
    }

    if (msg.kind === "vortex.getLogs") {
      sendResponse({ logs: ring.snapshot() });
      return false;
    }

    return false;
  });
});
