import fc from "fast-check";
import { describe, expect, it, vi } from "vitest";
import type { Envelope, ProtocolMessageMap } from "@vortex/protocol";
import {
  CaptureDecisionEngine,
  DEFAULT_CAPTURE_RULES,
  ExtensionRingBuffer,
  NativeBridgeClient,
  ReversedLabelRadixTrie,
  SafeSuggestGuard,
  collectMergedCookies,
  deriveTopLevelSite,
  generateUuidV7,
  handleDeterminingFilename,
} from "../src/index";

describe("ReversedLabelRadixTrie & CaptureDecisionEngine (P1-T27)", () => {
  it("matches exact and wildcard subdomains without false-matching sibling suffixes (property-based)", () => {
    const trie = new ReversedLabelRadixTrie([
      "localhost",
      "127.0.0.1",
      "*.internal.corp",
      "exact.org",
    ]);
    expect(trie.isExcluded("localhost")).toBe(true);
    expect(trie.isExcluded("internal.corp")).toBe(true);
    expect(trie.isExcluded("a.b.c.internal.corp")).toBe(true);
    expect(trie.isExcluded("evilinternal.corp")).toBe(false);
    expect(trie.isExcluded("exact.org")).toBe(true);
    expect(trie.isExcluded("sub.exact.org")).toBe(false);

    const labelArb = fc.stringMatching(/^[a-z0-9]{1,10}$/);
    fc.assert(
      fc.property(fc.array(labelArb, { minLength: 1, maxLength: 4 }), (subLabels) => {
        const subHost = `${subLabels.join(".")}.internal.corp`;
        const unrelatedHost = `${subLabels.join(".")}.external.net`;
        expect(trie.isExcluded(subHost)).toBe(true);
        expect(trie.isExcluded(unrelatedHost)).toBe(false);
      }),
    );
  });

  it("enforces Alt bypass, Incognito privacy, POST, blob:/data:, 1 MiB threshold, and MIME/ext match", () => {
    const engine = new CaptureDecisionEngine(DEFAULT_CAPTURE_RULES);

    expect(
      engine.evaluate({
        url: "https://releases.ubuntu.com/26.04/ubuntu.iso",
        totalBytes: 5_000_000_000,
        incognito: false,
        altKeyBypassActive: false,
      }),
    ).toEqual({ shouldCapture: true, reason: "matched" });

    expect(
      engine.evaluate({
        url: "https://releases.ubuntu.com/26.04/ubuntu.iso",
        totalBytes: 5_000_000_000,
        incognito: false,
        altKeyBypassActive: true,
      }).reason,
    ).toBe("alt_bypass");

    expect(
      engine.evaluate({
        url: "blob:https://example.com/123",
        totalBytes: 5_000_000,
        incognito: false,
        altKeyBypassActive: false,
      }).reason,
    ).toBe("forbidden_scheme");

    expect(
      engine.evaluate({
        url: "https://example.com/archive.zip",
        totalBytes: 5_000_000,
        incognito: false,
        method: "POST",
        altKeyBypassActive: false,
      }).reason,
    ).toBe("post_method");

    expect(
      engine.evaluate({
        url: "https://example.com/archive.zip",
        totalBytes: 5_000_000,
        incognito: true,
        altKeyBypassActive: false,
      }).reason,
    ).toBe("incognito_blocked");

    expect(
      engine.evaluate({
        url: "https://example.com/tiny.zip",
        totalBytes: 12_000,
        incognito: false,
        altKeyBypassActive: false,
      }).reason,
    ).toBe("below_min_size");
  });
});

describe("SafeSuggestGuard, CHIPS Cookies & Live-Stream Hold without downloads.pause() (P1-T28)", () => {
  it("merges unpartitioned and CHIPS partitioned cookies cleanly", async () => {
    expect(deriveTopLevelSite("https://portal.example.com/downloads?id=9")).toBe(
      "https://portal.example.com",
    );

    const mockCookiesApi = {
      getAll: vi.fn(async (details: chrome.cookies.GetAllDetails) => {
        if (details.partitionKey !== undefined) {
          return [{ name: "chips_token", value: "part_v2" } as chrome.cookies.Cookie];
        }
        return [
          { name: "session_id", value: "norm_v1" } as chrome.cookies.Cookie,
          { name: "chips_token", value: "old_unpartitioned" } as chrome.cookies.Cookie,
        ];
      }),
    };

    const header = await collectMergedCookies(
      mockCookiesApi,
      "https://cdn.example.net/file.iso",
      "https://portal.example.com",
    );
    expect(header).toBe("session_id=norm_v1; chips_token=part_v2");
  });

  it("always calls suggest() on 2.0s timeout and never fires more than once", async () => {
    vi.useFakeTimers();
    const suggest = vi.fn();
    let timedOut = false;
    const guard = new SafeSuggestGuard(suggest, 2_000, () => {
      timedOut = true;
    });

    vi.advanceTimersByTime(2_001);
    expect(timedOut).toBe(true);
    expect(suggest).toHaveBeenCalledTimes(1);
    expect(guard.releaseToBrowser()).toBe(false);
    expect(guard.markTakenOverByVortex()).toBe(false);
    expect(suggest).toHaveBeenCalledTimes(1);
    vi.useRealTimers();
  });

  it("cancels & erases on accepted=true (<80%) and seamlessly calls suggest() on WAF JA4 403 fallback WITHOUT pause()", async () => {
    const createMockBridge = (fallback: boolean): NativeBridgeClient => {
      const listeners: Array<(msg: unknown) => void> = [];
      const fakePort = {
        name: "com.vortex.bridge",
        postMessage: (env: Envelope) => {
          queueMicrotask(() => {
            if (env.type === "hello") {
              const ack: Envelope<"hello.ack"> = {
                v: 1,
                id: generateUuidV7(),
                replyTo: env.id,
                traceId: env.traceId,
                timestampMs: Date.now(),
                type: "hello.ack",
                payload: {
                  negotiatedVersion: 1,
                  coreVersion: "0.1.0",
                  capabilities: ["handoff"],
                  captureRules: DEFAULT_CAPTURE_RULES,
                },
              };
              for (const l of listeners) l(ack);
            } else if (env.type === "download.handoff") {
              const payload = env.payload as ProtocolMessageMap["download.handoff"];
              expect(payload.autoStart).toBe(false);
              const hAck: Envelope<"download.handoffAck"> = {
                v: 1,
                id: generateUuidV7(),
                replyTo: env.id,
                traceId: env.traceId,
                timestampMs: Date.now(),
                type: "download.handoffAck",
                payload: fallback
                  ? {
                      downloadId: "",
                      accepted: false,
                      duplicateFound: false,
                      fallbackToBrowser: true,
                      fallbackReason: "waf_tls_bound",
                    }
                  : {
                      downloadId: "0192f3a4-0000-7000-8000-000000000777",
                      accepted: true,
                      duplicateFound: false,
                      fallbackToBrowser: false,
                      fallbackReason: "",
                    },
              };
              for (const l of listeners) l(hAck);
            }
          });
        },
        disconnect: vi.fn(),
        onMessage: { addListener: (cb: (msg: unknown) => void) => listeners.push(cb) },
        onDisconnect: { addListener: vi.fn() },
      } as unknown as chrome.runtime.Port;

      return new NativeBridgeClient(
        { connect: () => fakePort },
        new CaptureDecisionEngine(DEFAULT_CAPTURE_RULES),
        new ExtensionRingBuffer(50),
      );
    };

    const baseItem = {
      id: 42,
      url: "https://cdn.example.com/big.iso?token=secret123",
      finalUrl: "https://cdn.example.com/big.iso?token=secret123",
      referrer: "https://example.com/dl",
      filename: "big.iso",
      mime: "application/x-iso9660-image",
      totalBytes: 100_000_000,
      bytesReceived: 1_000_000,
      incognito: false,
      state: "in_progress",
    } as unknown as chrome.downloads.DownloadItem;

    // Scenario A: Core accepts -> cancel() + erase(), suggest() NOT called!
    const suggestAccepted = vi.fn();
    const cancelFn = vi.fn(async () => undefined);
    const eraseFn = vi.fn(async () => [42]);
    const ringA = new ExtensionRingBuffer(50);

    const heldA = handleDeterminingFilename(baseItem, suggestAccepted, {
      engine: new CaptureDecisionEngine(DEFAULT_CAPTURE_RULES),
      bridge: createMockBridge(false),
      cookiesApi: { getAll: async () => [] },
      downloadsApi: {
        search: async () => [baseItem],
        cancel: cancelFn,
        erase: eraseFn,
      },
      ring: ringA,
      userAgent: "BraveTest/1.0",
      isAltBypassActive: () => false,
    });
    expect(heldA).toBe(true);
    await new Promise((r) => setTimeout(r, 30));
    expect(cancelFn).toHaveBeenCalledWith(42);
    expect(eraseFn).toHaveBeenCalledWith({ id: 42 });
    expect(suggestAccepted).not.toHaveBeenCalled();
    expect(ringA.toJsonl()).not.toContain("secret123");

    // Scenario B: Cloudflare JA4 403 / Single-Use Token -> `fallbackToBrowser: true` -> `suggest()` called on ORIGINAL stream!
    const suggestFallback = vi.fn();
    const cancelFallback = vi.fn(async () => undefined);
    const ringB = new ExtensionRingBuffer(50);

    handleDeterminingFilename(baseItem, suggestFallback, {
      engine: new CaptureDecisionEngine(DEFAULT_CAPTURE_RULES),
      bridge: createMockBridge(true),
      cookiesApi: { getAll: async () => [] },
      downloadsApi: {
        search: async () => [baseItem],
        cancel: cancelFallback,
        erase: async () => [],
      },
      ring: ringB,
      userAgent: "BraveTest/1.0",
      isAltBypassActive: () => false,
    });
    await new Promise((r) => setTimeout(r, 30));
    expect(suggestFallback).toHaveBeenCalledTimes(1);
    expect(cancelFallback).not.toHaveBeenCalled();

    // Verify unused ProtocolMessageMap type stays valid
    const _check: ProtocolMessageMap["ping"] = { sentAtMs: 1 };
    expect(_check.sentAtMs).toBe(1);
  });
});
