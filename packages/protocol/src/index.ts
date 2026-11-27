/**
 * Shared TypeScript IPC & Native Messaging v1 Contracts (`packages/protocol` — `P0-T14`).
 *
 * Mirrors `proto/vortex_ipc.proto` (`vortex.ipc.v1`) and `crates/vortex-error` 1:1 using
 * Channel A camelCase field names (`Protocol-Spec.md` §2.2).
 */

export const PROTOCOL_VERSION = 1 as const;
export const MIN_SUPPORTED_PROTOCOL_VERSION = 1 as const;
export const MAX_NATIVE_HOST_TO_EXT_BYTES = 1024 * 1024; // 1 MiB
export const MAX_PIPE_FRAME_BYTES = 4 * 1024 * 1024; // 4 MiB
export const SAFE_SUGGEST_GUARD_TIMEOUT_MS = 2000;
export const PROBE_BUDGET_MS = 1400;
export const MAX_URL_BYTES = 8192;
export const MAX_SUGGESTED_NAME_BYTES = 512;
export const MAX_HEADER_COUNT = 64;
export const MAX_HEADER_VALUE_BYTES = 8192;
export const MAX_COOKIES_BYTES = 64 * 1024;

export type ErrorCode =
  | "VTX-NET-001"
  | "VTX-NET-002"
  | "VTX-NET-003"
  | "VTX-NET-004"
  | "VTX-NET-005"
  | "VTX-RES-001"
  | "VTX-RES-002"
  | "VTX-RES-003"
  | "VTX-RES-004"
  | "VTX-RES-005"
  | "VTX-RES-006"
  | "VTX-AUTH-001"
  | "VTX-AUTH-002"
  | "VTX-DISK-001"
  | "VTX-DISK-002"
  | "VTX-DISK-003"
  | "VTX-DISK-004"
  | "VTX-DISK-005"
  | "VTX-DISK-006"
  | "VTX-INTG-001"
  | "VTX-DB-001"
  | "VTX-IPC-001"
  | "VTX-IPC-002"
  | "VTX-IPC-003"
  | "VTX-IPC-004"
  | "VTX-SEC-001"
  | "VTX-SEC-002"
  | "VTX-SEC-003"
  | "VTX-BUG-001";

export type ErrorCategory =
  | "Network"
  | "Resource"
  | "Auth"
  | "Disk"
  | "Integrity"
  | "Database"
  | "Ipc"
  | "Security"
  | "Internal";

export type RecoverabilityKind =
  "Transient" | "UserActionRequired" | "Permanent" | "SelfHealed" | "IsolatedBug";

export type SuggestedAction =
  | "Retry"
  | "RefreshLink"
  | "ChangeFolder"
  | "FreeDiskSpace"
  | "EnterCredentials"
  | "ReconnectDrive"
  | "RestartFromBeginning"
  | "UpdateVortex"
  | "ExportDiagnostics"
  | "CopyDiagnostics"
  | "CheckConnection"
  | "InspectUrl"
  | "RepairBrowserBridge"
  | "OpenReferrerInBrowser"
  | "SyncSystemClock"
  | "OpenWindowsSecurity";

export interface CaptureRules {
  readonly enabled: boolean;
  readonly minFileSizeBytes: number;
  readonly extensions: readonly string[];
  readonly mimeTypes: readonly string[];
  readonly excludedHosts: readonly string[];
  readonly captureInIncognito: boolean;
}

export interface HelloRequest {
  readonly clientKind:
    | "extension-brave"
    | "extension-chrome"
    | "extension-edge"
    | "extension-opera"
    | "extension-vivaldi"
    | "extension-firefox"
    | "cli";
  readonly clientVersion: string;
  readonly minSupportedV: number;
  readonly maxSupportedV: number;
  readonly capabilities: readonly string[];
}

export interface HelloAck {
  readonly negotiatedVersion: number;
  readonly coreVersion: string;
  readonly capabilities: readonly string[];
  readonly captureRules: CaptureRules;
}

export type HandoffSource = "determining-filename" | "context-menu" | "video-button" | "cli";

export interface DownloadHandoff {
  readonly url: string;
  readonly finalUrl: string;
  readonly referrer: string;
  readonly suggestedName: string;
  readonly mimeType: string;
  readonly sizeBytes?: number;
  readonly userAgent: string;
  readonly cookies: string;
  readonly headers: Readonly<Record<string, string>>;
  readonly incognito: boolean;
  readonly source: HandoffSource;
  readonly autoStart: boolean;
  readonly topLevelSite: string;
}

export interface DownloadHandoffAck {
  readonly downloadId: string;
  readonly accepted: boolean;
  readonly duplicateFound: boolean;
  readonly fallbackToBrowser: boolean;
  readonly fallbackReason: string;
}

export interface MediaStreamOption {
  readonly formatId: string;
  readonly label: string;
  readonly container: "mp4" | "mkv" | "mp3" | "m4a";
  readonly approxSizeBytes?: number;
  readonly audioOnly: boolean;
}

export interface MediaDetected {
  readonly pageUrl: string;
  readonly mediaUrl: string;
  readonly title: string;
  readonly mimeType: string;
  readonly options: readonly MediaStreamOption[];
}

export interface MediaDownloadRequest {
  readonly pageUrl: string;
  readonly mediaUrl: string;
  readonly formatId: string;
  readonly subtitleLangs: readonly string[];
  readonly userAgent: string;
  readonly cookies: string;
  readonly topLevelSite: string;
  readonly isPlaylist: boolean;
}

export interface LinkRefreshBegin {
  readonly downloadId: string;
  readonly referrerUrl: string;
  readonly expectedName: string;
  readonly expectedMime: string;
  readonly expectedSizeBytes?: number;
}

export interface LinkRefreshCandidate {
  readonly downloadId: string;
  readonly candidate: DownloadHandoff;
}

export interface ApiError {
  readonly code: ErrorCode;
  readonly category: ErrorCategory;
  readonly recoverability: RecoverabilityKind;
  readonly retryAfterMs?: number;
  readonly userMessageKey: string;
  readonly messageParams: Readonly<Record<string, string>>;
  readonly suggestedActions: readonly SuggestedAction[];
  readonly technicalDetail: string;
  readonly traceId: string;
}

export interface ProtocolMessageMap {
  readonly hello: HelloRequest;
  readonly "hello.ack": HelloAck;
  readonly ping: { readonly sentAtMs: number };
  readonly pong: { readonly echoSentAtMs: number };
  readonly "download.handoff": DownloadHandoff;
  readonly "download.handoffAck": DownloadHandoffAck;
  readonly "download.batch": { readonly items: readonly DownloadHandoff[] };
  readonly "media.detected": MediaDetected;
  readonly "media.download": MediaDownloadRequest;
  readonly "linkRefresh.begin": LinkRefreshBegin;
  readonly "linkRefresh.candidate": LinkRefreshCandidate;
  readonly "settings.changed": { readonly captureRules: CaptureRules };
  readonly "diagnostics.dumpReq": { readonly maxEntries: number };
  readonly "diagnostics.dumpResp": { readonly extensionLogsJsonl: string };
  readonly "health.checkReq": Record<string, never>;
  readonly "health.checkResp": {
    readonly coreOk: boolean;
    readonly dbOk: boolean;
    readonly pipeOk: boolean;
    readonly freeDiskBytes: number;
  };
  readonly bringToFront: { readonly openUrl?: string };
  readonly error: ApiError;
}

export type ProtocolMessageType = keyof ProtocolMessageMap;

export interface Envelope<T extends ProtocolMessageType = ProtocolMessageType> {
  readonly v: number;
  readonly id: string;
  readonly replyTo: string;
  readonly traceId: string;
  readonly timestampMs: number;
  readonly type: T;
  readonly payload: ProtocolMessageMap[T];
}

/** Validates whether a raw URL is permitted by the Vortex http/https allowlist (TH-03). */
export function isAllowedTransferUrl(rawUrl: string): boolean {
  const trimmed = rawUrl.trim();
  if (trimmed.length === 0 || trimmed.length > MAX_URL_BYTES) {
    return false;
  }
  if (trimmed.startsWith("\\\\") || trimmed.startsWith("//")) {
    return false;
  }
  const lower = trimmed.toLowerCase();
  return lower.startsWith("http://") || lower.startsWith("https://");
}
