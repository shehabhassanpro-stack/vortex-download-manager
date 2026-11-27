/**
 * Native Messaging Client (`com.vortex.bridge`) & Live-Stream Handoff Coordinator (`P1-T28`).
 *
 * Implements:
 * - Handshake (`hello` -> `hello.ack`) + `30s` `ping`/`pong` liveness
 * - Live `settings.changed` (`CaptureRules`) subscription
 * - `handleDeterminingFilename` WITHOUT `downloads.pause()` inside `SafeSuggestGuard` (`2.0s` budget):
 *   - `< 80%` progress + `accepted: true` -> `cancel(id)` + `erase({ id })`
 *   - `>= 80%` progress OR `fallbackToBrowser: true` OR timeout/disconnect -> `suggest()` on the
 *     **same open browser TCP socket**!
 */

import {
  PROTOCOL_VERSION,
  PROBE_BUDGET_MS,
  SAFE_SUGGEST_GUARD_TIMEOUT_MS,
  type CaptureRules,
  type DownloadHandoff,
  type DownloadHandoffAck,
  type Envelope,
  type HelloAck,
  type ProtocolMessageMap,
  type ProtocolMessageType,
} from "@vortex/protocol";
import type { CaptureDecisionEngine } from "./capture-engine";
import { collectMergedCookies, deriveTopLevelSite, type CookiesApiLike } from "./cookies";
import { ExtensionRingBuffer, generateUuidV7 } from "./ring-buffer";
import { SafeSuggestGuard, type SuggestFn } from "./safe-suggest-guard";

export const NATIVE_HOST_ID = "com.vortex.bridge";
export const NEARLY_DONE_RATIO = 0.8;

interface PendingRpc {
  readonly resolve: (env: Envelope) => void;
  readonly reject: (err: Error) => void;
  readonly timer: ReturnType<typeof setTimeout>;
}

export interface NativeBridgeTransport {
  connect(): chrome.runtime.Port;
}

export class NativeBridgeClient {
  private port: chrome.runtime.Port | undefined;
  private handshakePromise: Promise<HelloAck> | undefined;
  private readonly pending = new Map<string, PendingRpc>();
  private heartbeatTimer: ReturnType<typeof setInterval> | undefined;
  private connected = false;
  private readonly transport: NativeBridgeTransport;
  private readonly engine: CaptureDecisionEngine;
  private readonly ring: ExtensionRingBuffer;
  private readonly onRulesUpdated: ((rules: CaptureRules) => void) | undefined;

  public constructor(
    transport: NativeBridgeTransport,
    engine: CaptureDecisionEngine,
    ring: ExtensionRingBuffer,
    onRulesUpdated?: (rules: CaptureRules) => void,
  ) {
    this.transport = transport;
    this.engine = engine;
    this.ring = ring;
    this.onRulesUpdated = onRulesUpdated;
  }

  public isConnected(): boolean {
    return this.connected;
  }

  public async ensureHandshake(traceId: string): Promise<HelloAck> {
    if (this.connected && this.handshakePromise !== undefined) {
      return this.handshakePromise;
    }
    if (this.handshakePromise !== undefined) {
      return this.handshakePromise;
    }

    const promise = (async (): Promise<HelloAck> => {
      const port = this.openPort();
      const helloEnv = this.makeEnvelope("hello", traceId, {
        clientKind: "extension-brave",
        clientVersion: "0.1.0",
        minSupportedV: PROTOCOL_VERSION,
        maxSupportedV: PROTOCOL_VERSION,
        capabilities: ["handoff", "diag-dump", "health-check"],
      });

      const reply = await this.sendRpcOverPort(port, helloEnv, 1_600);
      if (reply.type === "error") {
        const errPayload = reply.payload as ProtocolMessageMap["error"];
        throw new Error(`${errPayload.code}: ${errPayload.technicalDetail}`);
      }
      if (reply.type !== "hello.ack") {
        throw new Error(`Unexpected handshake reply: ${reply.type}`);
      }
      const ack = reply.payload as HelloAck;
      this.connected = true;
      this.engine.updateRules(ack.captureRules);
      this.onRulesUpdated?.(ack.captureRules);
      this.startHeartbeat();
      return ack;
    })();

    this.handshakePromise = promise;
    try {
      return await promise;
    } catch (err) {
      this.teardownPort(err instanceof Error ? err : new Error(String(err)));
      throw err;
    }
  }

  public async sendDownloadHandoff(
    handoff: DownloadHandoff,
    traceId: string,
    timeoutMs: number = PROBE_BUDGET_MS + 350,
  ): Promise<DownloadHandoffAck> {
    await this.ensureHandshake(traceId);
    if (this.port === undefined) {
      throw new Error("VTX-IPC-001: Native port unavailable");
    }
    const env = this.makeEnvelope("download.handoff", traceId, handoff);
    const reply = await this.sendRpcOverPort(this.port, env, timeoutMs);
    if (reply.type === "error") {
      const apiErr = reply.payload as ProtocolMessageMap["error"];
      throw new Error(`${apiErr.code}: ${apiErr.technicalDetail}`);
    }
    if (reply.type !== "download.handoffAck") {
      throw new Error(`Unexpected handoff reply: ${reply.type}`);
    }
    return reply.payload as DownloadHandoffAck;
  }

  private openPort(): chrome.runtime.Port {
    if (this.port !== undefined) {
      return this.port;
    }
    const p = this.transport.connect();
    p.onMessage.addListener((raw: unknown) => {
      this.onIncomingMessage(raw);
    });
    p.onDisconnect.addListener(() => {
      const lastErr = chrome.runtime?.lastError?.message ?? "Native host disconnected";
      this.teardownPort(new Error(`VTX-IPC-001: ${lastErr}`));
    });
    this.port = p;
    return p;
  }

  private onIncomingMessage(raw: unknown): void {
    if (typeof raw !== "object" || raw === null) {
      return;
    }
    const env = raw as Envelope;
    if (env.type === "settings.changed") {
      const payload = env.payload as ProtocolMessageMap["settings.changed"];
      this.engine.updateRules(payload.captureRules);
      this.onRulesUpdated?.(payload.captureRules);
      this.ring.push("INFO", env.traceId, "settings.changed", "Updated live CaptureRules");
      return;
    }
    if (env.type === "diagnostics.dumpReq" && this.port !== undefined) {
      const req = env.payload as ProtocolMessageMap["diagnostics.dumpReq"];
      const resp = this.makeEnvelope(
        "diagnostics.dumpResp",
        env.traceId,
        { extensionLogsJsonl: this.ring.toJsonl(req.maxEntries) },
        env.id,
      );
      try {
        this.port.postMessage(resp);
      } catch {
        // Ignore broken port
      }
      return;
    }

    if (typeof env.replyTo === "string" && env.replyTo.length > 0) {
      const waiter = this.pending.get(env.replyTo);
      if (waiter !== undefined) {
        clearTimeout(waiter.timer);
        this.pending.delete(env.replyTo);
        waiter.resolve(env);
      }
    }
  }

  private sendRpcOverPort(
    port: chrome.runtime.Port,
    env: Envelope,
    timeoutMs: number,
  ): Promise<Envelope> {
    return new Promise<Envelope>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(env.id);
        reject(new Error(`VTX-IPC-001: Native Messaging timeout after ${timeoutMs}ms`));
      }, timeoutMs);

      this.pending.set(env.id, { resolve, reject, timer });
      try {
        port.postMessage(env);
      } catch (err) {
        clearTimeout(timer);
        this.pending.delete(env.id);
        reject(err instanceof Error ? err : new Error(String(err)));
      }
    });
  }

  private startHeartbeat(): void {
    if (this.heartbeatTimer !== undefined) {
      clearInterval(this.heartbeatTimer);
    }
    this.heartbeatTimer = setInterval(() => {
      if (!this.connected || this.port === undefined) {
        return;
      }
      const tid = generateUuidV7();
      const ping = this.makeEnvelope("ping", tid, { sentAtMs: Date.now() });
      void this.sendRpcOverPort(this.port, ping, 5_000).catch(() => {
        this.teardownPort(new Error("Heartbeat ping timed out"));
      });
    }, 30_000);
  }

  private teardownPort(reason: Error): void {
    this.connected = false;
    this.handshakePromise = undefined;
    if (this.heartbeatTimer !== undefined) {
      clearInterval(this.heartbeatTimer);
      this.heartbeatTimer = undefined;
    }
    if (this.port !== undefined) {
      try {
        this.port.disconnect();
      } catch {
        // Already disconnected
      }
      this.port = undefined;
    }
    for (const [id, waiter] of this.pending.entries()) {
      clearTimeout(waiter.timer);
      waiter.reject(reason);
      this.pending.delete(id);
    }
  }

  private makeEnvelope<T extends ProtocolMessageType>(
    type: T,
    traceId: string,
    payload: ProtocolMessageMap[T],
    replyTo = "",
  ): Envelope<T> {
    return {
      v: PROTOCOL_VERSION,
      id: generateUuidV7(),
      replyTo,
      traceId,
      timestampMs: Date.now(),
      type,
      payload,
    };
  }
}

export interface DownloadsApiLike {
  search(query: chrome.downloads.DownloadQuery): Promise<chrome.downloads.DownloadItem[]>;
  cancel(downloadId: number): Promise<void>;
  erase(query: chrome.downloads.DownloadQuery): Promise<number[]>;
}

export interface LiveHandoffDeps {
  readonly engine: CaptureDecisionEngine;
  readonly bridge: NativeBridgeClient;
  readonly cookiesApi: CookiesApiLike;
  readonly downloadsApi: DownloadsApiLike;
  readonly ring: ExtensionRingBuffer;
  readonly userAgent: string;
  readonly isAltBypassActive: () => boolean;
  readonly guardTimeoutMs?: number;
}

/**
 * Intercepts `chrome.downloads.onDeterminingFilename(item, suggest)`.
 *
 * Returns `true` when holding `suggest()` asynchronously inside `SafeSuggestGuard`, or `false`
 * when immediately releasing non-captured items to Brave/Chromium.
 *
 * **NEVER calls `chrome.downloads.pause()`!**
 */
export function handleDeterminingFilename(
  item: chrome.downloads.DownloadItem,
  suggest: SuggestFn,
  deps: LiveHandoffDeps,
): boolean {
  const decision = deps.engine.evaluate({
    url: item.url,
    ...(item.finalUrl.length > 0 ? { finalUrl: item.finalUrl } : {}),
    ...(item.filename.length > 0 ? { filename: item.filename } : {}),
    ...(item.mime.length > 0 ? { mime: item.mime } : {}),
    totalBytes: item.totalBytes,
    incognito: item.incognito,
    altKeyBypassActive: deps.isAltBypassActive(),
  });

  if (!decision.shouldCapture) {
    suggest();
    return false;
  }

  const traceId = generateUuidV7();
  const guard = new SafeSuggestGuard(
    suggest,
    deps.guardTimeoutMs ?? SAFE_SUGGEST_GUARD_TIMEOUT_MS,
    () => {
      deps.ring.push(
        "WARN",
        traceId,
        "handoff.guard_timeout",
        `SafeSuggestGuard 2.0s elapsed -> browser continues seamlessly for ${item.url}`,
      );
    },
  );

  void (async (): Promise<void> => {
    try {
      // Fast-file pre-check: if browser already received >= 80% before probe even starts, let it finish!
      if (item.totalBytes > 0 && item.bytesReceived / item.totalBytes >= NEARLY_DONE_RATIO) {
        deps.ring.push("INFO", traceId, "handoff.nearly_done", item.url);
        guard.releaseToBrowser();
        return;
      }

      const effectiveUrl = item.finalUrl.length > 0 ? item.finalUrl : item.url;
      const topLevelSite = deriveTopLevelSite(item.referrer);
      const mergedCookies = await collectMergedCookies(deps.cookiesApi, effectiveUrl, topLevelSite);

      const handoff: DownloadHandoff = {
        url: item.url,
        finalUrl: item.finalUrl,
        referrer: item.referrer,
        suggestedName: item.filename,
        mimeType: item.mime,
        ...(item.totalBytes > 0 ? { sizeBytes: item.totalBytes } : {}),
        userAgent: deps.userAgent,
        cookies: mergedCookies,
        headers: {},
        incognito: item.incognito,
        source: "determining-filename",
        autoStart: false,
        topLevelSite,
      };

      const ack = await deps.bridge.sendDownloadHandoff(handoff, traceId);
      if (guard.isSettled()) {
        return;
      }

      if (!ack.accepted || ack.fallbackToBrowser) {
        deps.ring.push(
          "WARN",
          traceId,
          "handoff.fallback_to_browser",
          `reason=${ack.fallbackReason || "declined"} url=${item.url}`,
        );
        guard.releaseToBrowser();
        return;
      }

      // Re-check live browser progress right before cancelling (`Protocol-Spec.md` §4: `< 80%` guard)!
      const latestItems = await deps.downloadsApi.search({ id: item.id });
      const latest = latestItems[0];
      if (
        latest !== undefined &&
        (latest.state === "complete" ||
          (latest.totalBytes > 0 && latest.bytesReceived / latest.totalBytes >= NEARLY_DONE_RATIO))
      ) {
        deps.ring.push("INFO", traceId, "handoff.browser_finished_during_probe", item.url);
        guard.releaseToBrowser();
        return;
      }

      if (guard.markTakenOverByVortex()) {
        await deps.downloadsApi.cancel(item.id);
        await deps.downloadsApi.erase({ id: item.id });
        deps.ring.push(
          "INFO",
          traceId,
          "handoff.accepted",
          `downloadId=${ack.downloadId} url=${item.url}`,
        );
      }
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      deps.ring.push("WARN", traceId, "handoff.error_fallback", `${msg} url=${item.url}`);
      guard.releaseToBrowser();
    } finally {
      if (!guard.isSettled()) {
        guard.releaseToBrowser();
      }
    }
  })();

  return true;
}
