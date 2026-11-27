/**
 * Redacted 500-Event Ring Buffer & UUIDv7 Trace Generator (`P1-T28` / `Protocol-Spec.md` §7).
 *
 * Persisted in `chrome.storage.session` across MV3 Service Worker sleep/wake cycles and
 * exported on `diagnostics.dumpReq` with zero unredacted URL tokens or credentials!
 */

export const MAX_RING_BUFFER_EVENTS = 500;

export interface ExtensionLogEvent {
  readonly tsMs: number;
  readonly level: "INFO" | "WARN" | "ERROR";
  readonly traceId: string;
  readonly event: string;
  readonly detail: string;
}

const SENSITIVE_PARAM_RE =
  /([?&](?:token|access_token|auth|sig|signature|key|api_key|secret|password|session|sid|jwt|hdnts|expires|code)=)[^&#\s"]+/gi;

export function redactForExtensionLog(raw: string): string {
  try {
    const u = new URL(raw);
    u.username = "";
    u.password = "";
    return u.toString().replace(SENSITIVE_PARAM_RE, "$1[REDACTED]");
  } catch {
    return raw.replace(/:\/\/[^/@\s]+@/g, "://").replace(SENSITIVE_PARAM_RE, "$1[REDACTED]");
  }
}

/**
 * Generates an RFC 9562 `UUIDv7` string (`48-bit` millisecond timestamp + `74-bit` CSPRNG).
 */
export function generateUuidV7(nowMs: number = Date.now()): string {
  const bytes = new Uint8Array(16);
  crypto.getRandomValues(bytes);

  const ts = BigInt(Math.max(0, Math.trunc(nowMs)));
  bytes[0] = Number((ts >> 40n) & 0xffn);
  bytes[1] = Number((ts >> 32n) & 0xffn);
  bytes[2] = Number((ts >> 24n) & 0xffn);
  bytes[3] = Number((ts >> 16n) & 0xffn);
  bytes[4] = Number((ts >> 8n) & 0xffn);
  bytes[5] = Number(ts & 0xffn);

  // Version 7 (`0111`) and RFC 4122 variant (`10xx`)
  bytes[6] = ((bytes[6] ?? 0) & 0x0f) | 0x70;
  bytes[8] = ((bytes[8] ?? 0) & 0x3f) | 0x80;

  const hex = Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20, 32)}`;
}

/**
 * Bounded 500-event ring buffer with automatic URL/secret scrubbing.
 */
export class ExtensionRingBuffer {
  private readonly capacity: number;
  private readonly entries: ExtensionLogEvent[] = [];

  public constructor(
    capacity: number = MAX_RING_BUFFER_EVENTS,
    seed: readonly ExtensionLogEvent[] = [],
  ) {
    this.capacity = Math.max(1, capacity);
    for (const item of seed.slice(-this.capacity)) {
      this.entries.push(item);
    }
  }

  public push(
    level: "INFO" | "WARN" | "ERROR",
    traceId: string,
    event: string,
    rawDetail: string,
    tsMs: number = Date.now(),
  ): ExtensionLogEvent {
    const clean: ExtensionLogEvent = {
      tsMs,
      level,
      traceId,
      event,
      detail: redactForExtensionLog(rawDetail),
    };
    if (this.entries.length >= this.capacity) {
      this.entries.shift();
    }
    this.entries.push(clean);
    return clean;
  }

  public snapshot(): readonly ExtensionLogEvent[] {
    return [...this.entries];
  }

  public toJsonl(maxEntries: number = this.capacity): string {
    return this.entries
      .slice(-Math.max(1, maxEntries))
      .map((e) => JSON.stringify(e))
      .join("\n");
  }
}
