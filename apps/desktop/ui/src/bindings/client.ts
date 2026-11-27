/**
 * Typed Frontend <-> Tauri 2 Core IPC Facade (`apps/desktop/ui/src/bindings/client.ts` — `P1-T30` & `P1.5`).
 *
 * Per `Engineering-Standards.md` §1.2, ONLY `src/bindings/` may import `invoke` from `@tauri-apps/api/core`.
 * Falls back cleanly to a deterministic preview/test adapter when `window.__TAURI_INTERNALS__` is absent
 * (e.g. `just dev-desktop` browser preview and `vitest` / `axe-core` SSR audits).
 */

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import type {
  ApiError,
  ErrorCategory,
  ErrorCode,
  RecoverabilityKind,
  SuggestedAction,
} from "@vortex/protocol";
import { generateDemoSegmentMap } from "../segmentMap.js";
import type {
  ApiError_Serialize as GeneratedApiError,
  ErrorCode as GeneratedErrorCode,
  RiskLevel as GeneratedRiskLevel,
  SecurityWarningKind as GeneratedSecurityWarningKind,
  SuggestedAction as GeneratedSuggestedAction,
} from "./generated.js";

// Compile-time zero-drift contract gate between Rust `tauri-specta` (`generated.ts`) & `@vortex/protocol` (`P1.6-T08`)!
type AssertExact<A, B> = [A] extends [B] ? ([B] extends [A] ? true : never) : never;
export type _ContractErrorCodeSync = AssertExact<ErrorCode, GeneratedErrorCode>;
export type _ContractSuggestedActionSync = AssertExact<SuggestedAction, GeneratedSuggestedAction>;
export type _ContractApiErrorFieldsSync = AssertExact<
  Exclude<keyof ApiError, "retryAfterMs">,
  keyof GeneratedApiError
>;

export type RiskLevel = GeneratedRiskLevel;

export type SecurityWarningKind = GeneratedSecurityWarningKind;

/**
 * Defensive wire-boundary normalizer guaranteeing `ApiError` ALWAYS has canonical `camelCase`
 * fields and a valid `suggestedActions` array even if a legacy `snake_case` SQLite row arrives (`P1.6-T08`).
 */
export function normalizeApiError(raw: unknown): ApiError {
  if (typeof raw !== "object" || raw === null) {
    return {
      code: "VTX-BUG-001",
      category: "Internal",
      recoverability: "IsolatedBug",
      userMessageKey: "errors.fallback",
      messageParams: {},
      suggestedActions: ["CopyDiagnostics"],
      technicalDetail: typeof raw === "string" ? raw : "Unknown error payload",
      traceId: "00000000-0000-0000-0000-000000000000",
    };
  }
  const rec = raw as Record<string, unknown>;
  const rawActions = rec["suggestedActions"] ?? rec["suggested_actions"];
  const suggestedActions: SuggestedAction[] = Array.isArray(rawActions)
    ? rawActions.filter((a): a is SuggestedAction => typeof a === "string")
    : ["CopyDiagnostics"];

  const rawParams = rec["messageParams"] ?? rec["message_params"];
  const messageParams: Record<string, string> = {};
  if (typeof rawParams === "object" && rawParams !== null) {
    for (const [k, v] of Object.entries(rawParams as Record<string, unknown>)) {
      if (typeof v === "string") {
        messageParams[k] = v;
      }
    }
  }

  let recoverability: RecoverabilityKind = "Permanent";
  let retryAfterMs: number | undefined;
  const rawRecov = rec["recoverability"];
  if (typeof rawRecov === "string") {
    recoverability = rawRecov as RecoverabilityKind;
  } else if (typeof rawRecov === "object" && rawRecov !== null) {
    const rObj = rawRecov as Record<string, unknown>;
    if (typeof rObj["kind"] === "string") {
      recoverability = rObj["kind"] as RecoverabilityKind;
    }
    const retryRaw = rObj["retryAfterMs"] ?? rObj["retry_after_ms"];
    if (typeof retryRaw === "number") {
      retryAfterMs = retryRaw;
    }
  }
  const topRetry = rec["retryAfterMs"] ?? rec["retry_after_ms"];
  if (retryAfterMs === undefined && typeof topRetry === "number") {
    retryAfterMs = topRetry;
  }

  return {
    code: (typeof rec["code"] === "string" ? rec["code"] : "VTX-BUG-001") as ErrorCode,
    category: (typeof rec["category"] === "string" ? rec["category"] : "Internal") as ErrorCategory,
    recoverability,
    ...(retryAfterMs !== undefined ? { retryAfterMs } : {}),
    userMessageKey:
      typeof rec["userMessageKey"] === "string"
        ? rec["userMessageKey"]
        : typeof rec["user_message_key"] === "string"
          ? rec["user_message_key"]
          : "errors.fallback",
    messageParams,
    suggestedActions,
    technicalDetail:
      typeof rec["technicalDetail"] === "string"
        ? rec["technicalDetail"]
        : typeof rec["technical_detail"] === "string"
          ? rec["technical_detail"]
          : "",
    traceId:
      typeof rec["traceId"] === "string"
        ? rec["traceId"]
        : typeof rec["trace_id"] === "string"
          ? rec["trace_id"]
          : "00000000-0000-0000-0000-000000000000",
  };
}

function normalizeSummary(dto: DownloadSummaryDto): DownloadSummaryDto {
  if (dto.error === null || dto.error === undefined) {
    return { ...dto, error: null };
  }
  return { ...dto, error: normalizeApiError(dto.error) };
}

export interface ContentSecurityAssessment {
  readonly risk_level: RiskLevel;
  readonly warnings: readonly SecurityWarningKind[];
  readonly detected_signature: string | null;
  readonly block_auto_start: boolean;
  readonly block_enter_shortcut: boolean;
  readonly origin_host: string;
  readonly referrer_host: string | null;
  readonly cross_origin_redirected: boolean;
}

export interface DownloadSummaryDto {
  readonly id: string;
  readonly file_name: string;
  readonly save_dir: string;
  readonly display_url: string;
  readonly total_size: number | null;
  readonly downloaded_bytes: number;
  readonly state_slug: string;
  readonly speed_bps: number;
  readonly eta_secs: number | null;
  readonly active_workers: number;
  readonly supports_range: boolean;
  readonly category_id: string;
  readonly queue_id: string;
  readonly priority: "Low" | "Normal" | "High" | "Critical";
  readonly created_at_ms: number;
  readonly completed_at_ms: number | null;
  readonly error: ApiError | null;
}

export interface ProgressSnapshotDto {
  readonly download_id: string;
  readonly state_slug: string;
  readonly downloaded_bytes: number;
  readonly total_size: number | null;
  readonly speed_fast_bps: number;
  readonly speed_slow_bps: number;
  readonly eta_secs: number | null;
  readonly active_workers: number;
  readonly bins: readonly number[];
  readonly protocol?: string | null;
  readonly concurrency_mode?: string | null;
}

export interface DownloadDetailsDto {
  readonly summary: DownloadSummaryDto;
  readonly referrer: string | null;
  readonly mime_type: string | null;
  readonly checksum: string | null;
  readonly bins: readonly number[];
}

export interface DuplicateMatchDto {
  readonly existing_id: string;
  readonly file_name: string;
  readonly save_dir: string;
  readonly state_slug: string;
  readonly file_exists_on_disk: boolean;
}

export interface AddDownloadPreviewDto {
  readonly staged_id: string;
  readonly display_url: string;
  readonly suggested_file_name: string;
  readonly category_id: string;
  readonly suggested_save_dir: string;
  readonly total_size: number | null;
  readonly supports_range: boolean;
  readonly free_disk_bytes: number | null;
  readonly security: ContentSecurityAssessment;
  readonly duplicate: DuplicateMatchDto | null;
  readonly canonical_url_transformed?: boolean;
  readonly url_provider_hint?: string | null;
}

export interface StagedDownloadDto {
  readonly staged_id: string;
  readonly url: string;
  readonly display_url: string;
  readonly suggested_file_name: string;
  readonly category_id: string;
  readonly suggested_save_dir: string;
  readonly total_size: number | null;
  readonly supports_range: boolean;
  readonly free_disk_bytes: number | null;
  readonly security: ContentSecurityAssessment;
  readonly duplicate: DuplicateMatchDto | null;
  readonly source: string;
  readonly burst_quarantined: boolean;
  readonly created_at_ms: number;
  readonly expires_at_ms: number;
}

export interface StagedChangedEventPayload {
  readonly pendingCount: number;
  readonly latestStagedId: string | null;
  readonly burstDetected: boolean;
}

export interface AddDownloadRequestDto {
  readonly stagedId?: string;
  readonly url: string;
  readonly fileName?: string;
  readonly saveDir?: string;
  readonly categoryId?: string;
  readonly expectedChecksum?: string;
  readonly autoStart: boolean;
  readonly overwriteExisting: boolean;
}

export interface CaptureRulesConfig {
  readonly enabled: boolean;
  readonly min_file_size_bytes: number;
  readonly extensions: readonly string[];
  readonly mime_types: readonly string[];
  readonly excluded_hosts: readonly string[];
  readonly capture_in_incognito: boolean;
}

export interface FeatureFlags {
  readonly enable_http3: boolean;
  readonly enable_ledbat: boolean;
  readonly enable_wasm_plugins: boolean;
  readonly enable_youtube_hybrid: boolean;
}

export interface AppConfigDto {
  readonly schema_version: number;
  readonly max_connections_per_download: number;
  readonly max_concurrent_downloads: number;
  readonly checkpoint_interval_secs: number;
  readonly idle_timeout_secs: number;
  readonly max_retries: number;
  readonly min_free_disk_bytes: number;
  readonly default_download_dir: string;
  readonly auto_start_handoff: boolean;
  readonly warm_instant_webview: boolean;
  readonly prevent_sleep_while_active: boolean;
  readonly clipboard_monitor_enabled: boolean;
  readonly theme: "system" | "light" | "dark";
  readonly language: "ar" | "en";
  readonly global_speed_limit_bps: number | null;
  readonly allow_private_network?: boolean;
  readonly auto_scale_connections?: boolean;
  readonly enable_http2?: boolean;
  readonly capture_rules: CaptureRulesConfig;
  readonly feature_flags: FeatureFlags;
}

export interface BrowserIntegrationStatusDto {
  readonly browser: string;
  readonly registered: boolean;
  readonly manifest_path: string;
}

export interface HealthStatusDto {
  readonly core_ok: boolean;
  readonly db_ok: boolean;
  readonly pipe_ok: boolean;
  readonly free_disk_bytes: number;
  readonly quarantined_records: number;
  readonly debug_mode_active: boolean;
  readonly browsers: readonly BrowserIntegrationStatusDto[];
}

export interface DiagnosticFilePreviewDto {
  readonly name: string;
  readonly size_bytes: number;
  readonly preview_excerpt: string;
}

export interface DiagnosticBundleReportDto {
  readonly bundle_path: string;
  readonly files: readonly DiagnosticFilePreviewDto[];
  readonly total_zip_bytes: number;
}

export interface LogEntryDto {
  readonly timestamp: string;
  readonly level: string;
  readonly target: string;
  readonly message: string;
  readonly trace_id: string | null;
}

export interface DownloadFilterDto {
  readonly state_slug?: string;
  readonly category_id?: string;
  readonly queue_id?: string;
}

export function isTauriRuntime(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

const SAFE_DEFAULT_SECURITY: ContentSecurityAssessment = {
  risk_level: "verified_match",
  warnings: [],
  detected_signature: "ZIP",
  block_auto_start: false,
  block_enter_shortcut: false,
  origin_host: "releases.ubuntu.com",
  referrer_host: null,
  cross_origin_redirected: false,
};

const DEMO_FALLBACK_DOWNLOADS: DownloadSummaryDto[] = [
  {
    id: "0192f3a4-0000-7000-8000-000000000101",
    file_name: "ubuntu-26.04-desktop-amd64.iso",
    save_dir: "D:/Downloads/Compressed",
    display_url: "https://releases.ubuntu.com/26.04/ubuntu-26.04-desktop-amd64.iso",
    total_size: 5_476_083_302,
    downloaded_bytes: 3_942_779_977,
    state_slug: "downloading",
    speed_bps: 25_952_256,
    eta_secs: 59,
    active_workers: 8,
    supports_range: true,
    category_id: "compressed",
    queue_id: "main",
    priority: "High",
    created_at_ms: 1_764_000_000_000,
    completed_at_ms: null,
    error: null,
  },
  {
    id: "0192f3a4-0000-7000-8000-000000000102",
    file_name: "setup-tool.exe",
    save_dir: "D:/Downloads/Programs",
    display_url: "https://cdn.example.com/setup-tool.exe?token=[REDACTED]",
    total_size: 125_829_120,
    downloaded_bytes: 62_914_560,
    state_slug: "needs_attention",
    speed_bps: 0,
    eta_secs: null,
    active_workers: 0,
    supports_range: true,
    category_id: "programs",
    queue_id: "main",
    priority: "Normal",
    created_at_ms: 1_764_000_010_000,
    completed_at_ms: null,
    error: {
      code: "VTX-RES-002",
      category: "Resource",
      recoverability: "UserActionRequired",
      userMessageKey: "errors.res.link_expired",
      messageParams: { host: "cdn.example.com" },
      suggestedActions: ["RefreshLink", "CopyDiagnostics"],
      technicalDetail: "HTTP 403 Forbidden — signed query token expired (redacted)",
      traceId: "0192f3a4-88b1-7000-8000-000000000042",
    },
  },
  {
    id: "0192f3a4-0000-7000-8000-000000000103",
    file_name: "محاضرة-هندسة-البرمجيات-2026.pdf",
    save_dir: "D:/Downloads/Documents",
    display_url: "https://edu.example.org/lectures/se-2026.pdf",
    total_size: 18_874_368,
    downloaded_bytes: 18_874_368,
    state_slug: "completed",
    speed_bps: 0,
    eta_secs: null,
    active_workers: 0,
    supports_range: true,
    category_id: "documents",
    queue_id: "main",
    priority: "Normal",
    created_at_ms: 1_764_000_020_000,
    completed_at_ms: 1_764_000_030_000,
    error: null,
  },
];

let demoStore = [...DEMO_FALLBACK_DOWNLOADS];
let demoStaged: StagedDownloadDto[] = [];
let demoTrash: DownloadSummaryDto | null = null;
let demoDebug = false;
let demoSettings: AppConfigDto = {
  schema_version: 1,
  max_connections_per_download: 8,
  max_concurrent_downloads: 3,
  checkpoint_interval_secs: 20,
  idle_timeout_secs: 30,
  max_retries: 10,
  min_free_disk_bytes: 268_435_456,
  default_download_dir: "C:/Users/User/Downloads",
  auto_start_handoff: false,
  warm_instant_webview: false,
  prevent_sleep_while_active: true,
  clipboard_monitor_enabled: true,
  theme: "dark",
  language: "ar",
  global_speed_limit_bps: null,
  auto_scale_connections: true,
  enable_http2: true,
  capture_rules: {
    enabled: true,
    min_file_size_bytes: 1_048_576,
    extensions: ["zip", "rar", "7z", "iso", "exe", "msi", "mp4", "mkv", "pdf"],
    mime_types: ["application/zip", "application/pdf", "video/mp4"],
    excluded_hosts: ["localhost", "127.0.0.1"],
    capture_in_incognito: false,
  },
  feature_flags: {
    enable_http3: false,
    enable_ledbat: false,
    enable_wasm_plugins: false,
    enable_youtube_hybrid: false,
  },
};

export const vortexApi = {
  async listDownloads(filter?: DownloadFilterDto): Promise<DownloadSummaryDto[]> {
    if (isTauriRuntime()) {
      const list = await invoke<DownloadSummaryDto[]>("cmd_list_downloads", {
        filter: filter ?? null,
      });
      return list.map(normalizeSummary);
    }
    return demoStore.filter((item) => {
      if (filter?.state_slug !== undefined && item.state_slug !== filter.state_slug) {
        return false;
      }
      if (filter?.category_id !== undefined && item.category_id !== filter.category_id) {
        return false;
      }
      return true;
    });
  },

  async searchDownloads(query: string): Promise<DownloadSummaryDto[]> {
    if (isTauriRuntime()) {
      const list = await invoke<DownloadSummaryDto[]>("cmd_search_downloads", { query });
      return list.map(normalizeSummary);
    }
    const q = query.trim().toLowerCase();
    if (q.length === 0) {
      return [...demoStore];
    }
    return demoStore.filter(
      (d) => d.file_name.toLowerCase().includes(q) || d.display_url.toLowerCase().includes(q),
    );
  },

  async prepareAddDownload(url: string): Promise<AddDownloadPreviewDto> {
    if (isTauriRuntime()) {
      return invoke<AddDownloadPreviewDto>("cmd_prepare_add_download", { url });
    }
    const parts = url.split("?")[0]?.split("/") ?? [];
    const name = parts[parts.length - 1] || "download.bin";
    const dup = demoStore.find((d) => d.file_name.toLowerCase() === name.toLowerCase()) ?? null;
    let host = "example.com";
    try {
      host = new URL(url).hostname;
    } catch {
      // keep default
    }
    return {
      staged_id: `0192f3a4-0000-7000-8000-${String(Date.now()).slice(-12)}`,
      display_url: url.split("?")[0] ?? url,
      suggested_file_name: name,
      category_id: "compressed",
      suggested_save_dir: demoSettings.default_download_dir,
      total_size: 268_435_456,
      supports_range: true,
      free_disk_bytes: 442_381_631_488,
      security: {
        ...SAFE_DEFAULT_SECURITY,
        origin_host: host,
      },
      duplicate:
        dup === null
          ? null
          : {
              existing_id: dup.id,
              file_name: dup.file_name,
              save_dir: dup.save_dir,
              state_slug: dup.state_slug,
              file_exists_on_disk: dup.state_slug === "completed",
            },
    };
  },

  async addDownload(req: AddDownloadRequestDto): Promise<DownloadSummaryDto> {
    if (isTauriRuntime()) {
      return invoke<DownloadSummaryDto>("cmd_add_download", { req });
    }
    if (req.stagedId !== undefined) {
      demoStaged = demoStaged.filter((s) => s.staged_id !== req.stagedId);
    }
    const created: DownloadSummaryDto = {
      id: req.stagedId ?? `0192f3a4-0000-7000-8000-${String(Date.now()).slice(-12)}`,
      file_name: req.fileName ?? "new-download.bin",
      save_dir: req.saveDir ?? demoSettings.default_download_dir,
      display_url: req.url,
      total_size: 104_857_600,
      downloaded_bytes: 0,
      state_slug: req.autoStart ? "downloading" : "paused",
      speed_bps: req.autoStart ? 12_582_912 : 0,
      eta_secs: req.autoStart ? 8 : null,
      active_workers: req.autoStart ? 8 : 0,
      supports_range: true,
      category_id: req.categoryId ?? "others",
      queue_id: "main",
      priority: "Normal",
      created_at_ms: Date.now(),
      completed_at_ms: null,
      error: null,
    };
    demoStore = [created, ...demoStore];
    return created;
  },

  async listStagedDownloads(): Promise<StagedDownloadDto[]> {
    if (isTauriRuntime()) {
      return invoke<StagedDownloadDto[]>("cmd_list_staged_downloads");
    }
    return [...demoStaged];
  },

  async discardStagedDownload(stagedId: string): Promise<boolean> {
    if (isTauriRuntime()) {
      return invoke<boolean>("cmd_discard_staged_download", { stagedId });
    }
    const before = demoStaged.length;
    demoStaged = demoStaged.filter((s) => s.staged_id !== stagedId);
    return demoStaged.length < before;
  },

  async discardAllStagedDownloads(): Promise<number> {
    if (isTauriRuntime()) {
      return invoke<number>("cmd_discard_all_staged_downloads");
    }
    const count = demoStaged.length;
    demoStaged = [];
    return count;
  },

  async blockCaptureHost(stagedId: string, host: string): Promise<AppConfigDto> {
    if (isTauriRuntime()) {
      return invoke<AppConfigDto>("cmd_block_capture_host", { stagedId, host });
    }
    const norm = host.trim().toLowerCase();
    if (!demoSettings.capture_rules.excluded_hosts.includes(norm)) {
      demoSettings = {
        ...demoSettings,
        capture_rules: {
          ...demoSettings.capture_rules,
          excluded_hosts: [...demoSettings.capture_rules.excluded_hosts, norm],
        },
      };
    }
    demoStaged = demoStaged.filter(
      (s) => s.staged_id !== stagedId && s.security.origin_host.toLowerCase() !== norm,
    );
    return demoSettings;
  },

  async getFreeDiskBytes(dir: string): Promise<number | null> {
    if (isTauriRuntime()) {
      return invoke<number | null>("cmd_free_disk_bytes", { dir });
    }
    return 442_381_631_488;
  },

  async pickFolder(initialDir?: string): Promise<string | null> {
    if (isTauriRuntime()) {
      return invoke<string | null>("cmd_pick_folder", { initialDir: initialDir ?? null });
    }
    return initialDir ?? demoSettings.default_download_dir;
  },

  async revealInFolder(id: string): Promise<void> {
    if (isTauriRuntime()) {
      await invoke("cmd_reveal_in_folder", { id });
    }
  },

  async openDownloadedFile(id: string): Promise<void> {
    if (isTauriRuntime()) {
      await invoke("cmd_open_downloaded_file", { id });
    }
  },

  async openDownloadReferrer(id: string): Promise<void> {
    if (isTauriRuntime()) {
      await invoke("cmd_open_download_referrer", { id });
    }
  },

  async openExternalUrl(url: string): Promise<void> {
    if (isTauriRuntime()) {
      await invoke("cmd_open_external_url", { url });
    }
  },

  async openMainWindow(): Promise<void> {
    if (isTauriRuntime()) {
      await invoke("cmd_open_main_window");
    }
  },

  async closeCurrentWindow(): Promise<void> {
    if (isTauriRuntime()) {
      await getCurrentWindow().close();
    }
  },

  async getSettings(): Promise<AppConfigDto> {
    if (isTauriRuntime()) {
      return invoke<AppConfigDto>("cmd_get_settings");
    }
    return demoSettings;
  },

  async updateSettings(newConfig: AppConfigDto): Promise<AppConfigDto> {
    if (isTauriRuntime()) {
      return invoke<AppConfigDto>("cmd_update_settings", { newConfig });
    }
    demoSettings = newConfig;
    return demoSettings;
  },

  async getDownloadDetails(id: string): Promise<DownloadDetailsDto> {
    if (isTauriRuntime()) {
      const details = await invoke<DownloadDetailsDto>("cmd_get_download_details", { id });
      return {
        ...details,
        summary: normalizeSummary(details.summary),
      };
    }
    const found = demoStore.find((d) => d.id === id) ?? demoStore[0];
    if (found === undefined) {
      throw new Error("Download not found");
    }
    return {
      summary: found,
      referrer: "https://releases.ubuntu.com/",
      mime_type: "application/octet-stream",
      checksum: null,
      bins: Array.from(generateDemoSegmentMap(8)),
    };
  },

  async pauseDownload(id: string): Promise<void> {
    if (isTauriRuntime()) {
      await invoke("cmd_pause_download", { id });
      return;
    }
    demoStore = demoStore.map((d) =>
      d.id === id ? { ...d, state_slug: "paused", speed_bps: 0, active_workers: 0 } : d,
    );
  },

  async pauseAll(): Promise<void> {
    if (isTauriRuntime()) {
      await invoke("cmd_pause_all");
      return;
    }
    demoStore = demoStore.map((d) =>
      d.state_slug === "downloading"
        ? { ...d, state_slug: "paused", speed_bps: 0, active_workers: 0 }
        : d,
    );
  },

  async resumeDownload(id: string): Promise<void> {
    if (isTauriRuntime()) {
      await invoke("cmd_resume_download", { id });
      return;
    }
    demoStore = demoStore.map((d) =>
      d.id === id
        ? { ...d, state_slug: "downloading", speed_bps: 16_777_216, active_workers: 8 }
        : d,
    );
  },

  async cancelDownload(id: string, deletePartialFiles: boolean): Promise<void> {
    if (isTauriRuntime()) {
      await invoke("cmd_cancel_download", { id, deletePartialFiles });
      return;
    }
    demoStore = demoStore.map((d) =>
      d.id === id ? { ...d, state_slug: "paused", speed_bps: 0 } : d,
    );
  },

  async removeDownload(id: string, deleteCompletedFile: boolean): Promise<void> {
    if (isTauriRuntime()) {
      await invoke("cmd_remove_download", { id, deleteCompletedFile });
      return;
    }
    demoTrash = demoStore.find((d) => d.id === id) ?? null;
    demoStore = demoStore.filter((d) => d.id !== id);
  },

  async undoRemoveDownload(id: string): Promise<DownloadSummaryDto> {
    if (isTauriRuntime()) {
      return invoke<DownloadSummaryDto>("cmd_undo_remove_download", { id });
    }
    if (demoTrash !== null && demoTrash.id === id) {
      demoStore = [demoTrash, ...demoStore];
      const restored = demoTrash;
      demoTrash = null;
      return restored;
    }
    throw new Error("Undo window expired");
  },

  async getHealthStatus(): Promise<HealthStatusDto> {
    if (isTauriRuntime()) {
      return invoke<HealthStatusDto>("cmd_get_health_status");
    }
    return {
      core_ok: true,
      db_ok: true,
      pipe_ok: true,
      free_disk_bytes: 442_381_631_488,
      quarantined_records: 0,
      debug_mode_active: demoDebug,
      browsers: [
        {
          browser: "Brave",
          registered: true,
          manifest_path: "<USERPROFILE>/AppData/Local/Vortex/manifests/com.vortex.bridge.json",
        },
        {
          browser: "Chrome",
          registered: true,
          manifest_path: "<USERPROFILE>/AppData/Local/Vortex/manifests/com.vortex.bridge.json",
        },
        {
          browser: "Edge",
          registered: true,
          manifest_path: "<USERPROFILE>/AppData/Local/Vortex/manifests/com.vortex.bridge.json",
        },
      ],
    };
  },

  async previewDiagnosticBundle(): Promise<DiagnosticBundleReportDto> {
    if (isTauriRuntime()) {
      return invoke<DiagnosticBundleReportDto>("cmd_preview_diagnostic_bundle");
    }
    return {
      bundle_path: "vortex-diagnostics-preview.zip",
      total_zip_bytes: 4_812,
      files: [
        {
          name: "system_info.json",
          size_bytes: 612,
          preview_excerpt: '{\n  "os": "windows",\n  "db_quick_check_ok": true\n}',
        },
        {
          name: "config_redacted.json",
          size_bytes: 840,
          preview_excerpt:
            '{\n  "max_connections_per_download": 8,\n  "default_download_dir": "<USERPROFILE>/Downloads"\n}',
        },
        {
          name: "logs/flight_recorder.jsonl",
          size_bytes: 2_140,
          preview_excerpt:
            '{"level":"INFO","target":"vortex_engine","message":"Checkpoint committed"}',
        },
      ],
    };
  },

  async exportDiagnosticBundle(destZipPath?: string): Promise<DiagnosticBundleReportDto> {
    if (isTauriRuntime()) {
      return invoke<DiagnosticBundleReportDto>("cmd_export_diagnostic_bundle", {
        destZipPath: destZipPath ?? null,
      });
    }
    return this.previewDiagnosticBundle();
  },

  async getRecentLogs(limit = 200): Promise<LogEntryDto[]> {
    if (isTauriRuntime()) {
      return invoke<LogEntryDto[]>("cmd_get_recent_logs", { limit });
    }
    return [
      {
        timestamp: "2026-11-26T12:00:01Z",
        level: "INFO",
        target: "vortex_engine::supervisor",
        message: "FSCTL_SET_SPARSE enabled; 8 adaptive Keep-Alive leases active",
        trace_id: "0192f3a4-88b1-7000-8000-000000000001",
      },
      {
        timestamp: "2026-11-26T12:00:05Z",
        level: "WARN",
        target: "vortex_net::client",
        message:
          "HTTP 403 signed token expired on https://cdn.example.com/setup-tool.exe?token=[REDACTED]",
        trace_id: "0192f3a4-88b1-7000-8000-000000000042",
      },
    ];
  },

  async setDebugMode(enabled: boolean): Promise<boolean> {
    if (isTauriRuntime()) {
      return invoke<boolean>("cmd_set_debug_mode", { enabled });
    }
    demoDebug = enabled;
    return enabled;
  },

  async repairBrowserBridge(): Promise<BrowserIntegrationStatusDto[]> {
    if (isTauriRuntime()) {
      return invoke<BrowserIntegrationStatusDto[]>("cmd_repair_browser_bridge");
    }
    return (await this.getHealthStatus()).browsers.map((b) => ({ ...b, registered: true }));
  },

  async logFrontendError(component: string, message: string): Promise<string> {
    if (isTauriRuntime()) {
      return invoke<string>("cmd_log_frontend_error", { component, message });
    }
    return "0192f3a4-0000-7000-8000-000000000999";
  },

  async onProgressBatch(
    handler: (batch: readonly ProgressSnapshotDto[]) => void,
  ): Promise<UnlistenFn> {
    if (isTauriRuntime()) {
      return listen<ProgressSnapshotDto[]>("vortex://progress-batch", (ev) => handler(ev.payload));
    }
    return () => undefined;
  },

  async onDomainEvent(handler: (downloadId: string) => void): Promise<UnlistenFn> {
    if (isTauriRuntime()) {
      return listen<string>("vortex://domain-event", (ev) => handler(ev.payload));
    }
    return () => undefined;
  },

  async onOpenUrl(handler: (url: string) => void): Promise<UnlistenFn> {
    if (isTauriRuntime()) {
      return listen<string>("vortex://open-url", (ev) => handler(ev.payload));
    }
    return () => undefined;
  },

  async onStagedDownloadsChanged(
    handler: (payload: StagedChangedEventPayload) => void,
  ): Promise<UnlistenFn> {
    if (isTauriRuntime()) {
      return listen<StagedChangedEventPayload>("vortex://staged-changed", (ev) =>
        handler(ev.payload),
      );
    }
    return () => undefined;
  },
};
