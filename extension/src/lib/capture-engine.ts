/**
 * Capture Decision Engine (`P1-T27` / `Vortex-Architecture-Design.md` §14.4).
 *
 * Implements:
 * - `ReversedLabelRadixTrie` for exact & wildcard (`*.internal.corp`) host exclusions in `O(labels)` time
 * - `Set<string>` lookup for file extensions and MIME types
 * - `1 MiB` minimum size threshold
 * - `Alt` key bypass (`<= 3,000ms` window)
 * - Strict exclusion of `blob:`, `data:`, `POST` downloads, and Incognito windows (unless explicitly opted in).
 */

import type { CaptureRules } from "@vortex/protocol";

interface TrieNode {
  exactExcluded: boolean;
  wildcardExcluded: boolean;
  readonly children: Map<string, TrieNode>;
}

function createNode(): TrieNode {
  return {
    exactExcluded: false,
    wildcardExcluded: false,
    children: new Map<string, TrieNode>(),
  };
}

function normalizeHostLabels(rawHost: string): readonly string[] {
  const trimmed = rawHost.trim().toLowerCase().replace(/\.+$/, "");
  if (trimmed.length === 0) {
    return [];
  }
  const withoutPort = trimmed.startsWith("[") ? trimmed : (trimmed.split(":")[0] ?? trimmed);
  return withoutPort
    .split(".")
    .filter((label) => label.length > 0)
    .reverse();
}

/**
 * Reversed-Label Radix/Label Trie matching exact hosts (`localhost`, `internal.corp`)
 * and wildcard domain suffixes (`*.internal.corp`) without regex backtracking (`TH-14`).
 */
export class ReversedLabelRadixTrie {
  private readonly root: TrieNode = createNode();

  public constructor(patterns: readonly string[] = []) {
    for (const pattern of patterns) {
      this.insert(pattern);
    }
  }

  public insert(rawPattern: string): void {
    const cleaned = rawPattern.trim().toLowerCase();
    if (cleaned.length === 0) {
      return;
    }

    const isWildcard = cleaned.startsWith("*.");
    const hostPart = isWildcard ? cleaned.slice(2) : cleaned;
    const reversedLabels = normalizeHostLabels(hostPart);
    if (reversedLabels.length === 0) {
      return;
    }

    let cursor = this.root;
    for (const label of reversedLabels) {
      let next = cursor.children.get(label);
      if (next === undefined) {
        next = createNode();
        cursor.children.set(label, next);
      }
      cursor = next;
    }

    if (isWildcard) {
      cursor.wildcardExcluded = true;
      cursor.exactExcluded = true;
    } else {
      cursor.exactExcluded = true;
    }
  }

  public isExcluded(hostname: string): boolean {
    const reversedLabels = normalizeHostLabels(hostname);
    if (reversedLabels.length === 0) {
      return false;
    }

    let cursor: TrieNode | undefined = this.root;
    for (let i = 0; i < reversedLabels.length; i += 1) {
      const label = reversedLabels[i];
      if (label === undefined) {
        break;
      }
      cursor = cursor.children.get(label);
      if (cursor === undefined) {
        return false;
      }
      if (cursor.wildcardExcluded) {
        return true;
      }
      if (i === reversedLabels.length - 1 && cursor.exactExcluded) {
        return true;
      }
    }
    return false;
  }
}

export interface DownloadCandidateInput {
  readonly url: string;
  readonly finalUrl?: string;
  readonly filename?: string;
  readonly mime?: string;
  readonly totalBytes: number;
  readonly incognito: boolean;
  readonly method?: string;
  readonly altKeyBypassActive: boolean;
}

export interface CaptureDecision {
  readonly shouldCapture: boolean;
  readonly reason:
    | "matched"
    | "disabled"
    | "alt_bypass"
    | "incognito_blocked"
    | "forbidden_scheme"
    | "post_method"
    | "excluded_host"
    | "below_min_size"
    | "unmatched_type";
}

export const DEFAULT_CAPTURE_RULES: CaptureRules = {
  enabled: true,
  minFileSizeBytes: 1_048_576,
  extensions: [
    "zip",
    "rar",
    "7z",
    "iso",
    "img",
    "tar",
    "gz",
    "xz",
    "zst",
    "exe",
    "msi",
    "msix",
    "apk",
    "dmg",
    "pkg",
    "deb",
    "rpm",
    "mp4",
    "mkv",
    "webm",
    "avi",
    "mov",
    "mp3",
    "m4a",
    "flac",
    "wav",
    "opus",
    "pdf",
    "epub",
    "bin",
  ],
  mimeTypes: [
    "application/zip",
    "application/x-7z-compressed",
    "application/vnd.rar",
    "application/x-iso9660-image",
    "application/x-msdownload",
    "application/x-msi",
    "application/octet-stream",
    "application/pdf",
    "video/mp4",
    "video/x-matroska",
    "audio/mpeg",
    "audio/flac",
  ],
  excludedHosts: ["localhost", "127.0.0.1"],
  captureInIncognito: false,
};

function extractExtension(urlStr: string, explicitFilename?: string): string {
  const source =
    explicitFilename !== undefined && explicitFilename.trim().length > 0
      ? explicitFilename.trim()
      : (() => {
          try {
            const parsed = new URL(urlStr);
            const segments = parsed.pathname.split("/");
            return segments[segments.length - 1] ?? "";
          } catch {
            return "";
          }
        })();

  const dotIdx = source.lastIndexOf(".");
  if (dotIdx <= 0 || dotIdx === source.length - 1) {
    return "";
  }
  return source.slice(dotIdx + 1).toLowerCase();
}

function normalizeMime(rawMime?: string): string {
  if (rawMime === undefined) {
    return "";
  }
  return (rawMime.split(";")[0] ?? "").trim().toLowerCase();
}

/**
 * Compiled Capture Decision Engine evaluated synchronously inside `onDeterminingFilename`.
 */
export class CaptureDecisionEngine {
  private rules: CaptureRules;
  private hostTrie: ReversedLabelRadixTrie;
  private extensionSet: Set<string>;
  private mimeSet: Set<string>;

  public constructor(rules: CaptureRules = DEFAULT_CAPTURE_RULES) {
    this.rules = rules;
    this.hostTrie = new ReversedLabelRadixTrie(rules.excludedHosts);
    this.extensionSet = new Set(rules.extensions.map((e) => e.trim().toLowerCase()));
    this.mimeSet = new Set(rules.mimeTypes.map((m) => normalizeMime(m)));
  }

  public updateRules(nextRules: CaptureRules): void {
    this.rules = nextRules;
    this.hostTrie = new ReversedLabelRadixTrie(nextRules.excludedHosts);
    this.extensionSet = new Set(nextRules.extensions.map((e) => e.trim().toLowerCase()));
    this.mimeSet = new Set(nextRules.mimeTypes.map((m) => normalizeMime(m)));
  }

  public currentRules(): CaptureRules {
    return this.rules;
  }

  public evaluate(candidate: DownloadCandidateInput): CaptureDecision {
    if (!this.rules.enabled) {
      return { shouldCapture: false, reason: "disabled" };
    }
    if (candidate.altKeyBypassActive) {
      return { shouldCapture: false, reason: "alt_bypass" };
    }
    if (candidate.incognito && !this.rules.captureInIncognito) {
      return { shouldCapture: false, reason: "incognito_blocked" };
    }
    if (candidate.method !== undefined && candidate.method.toUpperCase() === "POST") {
      return { shouldCapture: false, reason: "post_method" };
    }

    const effectiveUrl =
      candidate.finalUrl !== undefined && candidate.finalUrl.length > 0
        ? candidate.finalUrl
        : candidate.url;

    let parsedUrl: URL;
    try {
      parsedUrl = new URL(effectiveUrl);
    } catch {
      return { shouldCapture: false, reason: "forbidden_scheme" };
    }

    if (parsedUrl.protocol !== "http:" && parsedUrl.protocol !== "https:") {
      // Explicitly ignores `blob:`, `data:`, `file:`, `chrome-extension:`, `javascript:` (`P1-T27`)
      return { shouldCapture: false, reason: "forbidden_scheme" };
    }

    if (this.hostTrie.isExcluded(parsedUrl.hostname)) {
      return { shouldCapture: false, reason: "excluded_host" };
    }

    // Known byte length (`> 0`) below `minFileSizeBytes` stays in browser for instant completion
    if (candidate.totalBytes > 0 && candidate.totalBytes < this.rules.minFileSizeBytes) {
      return { shouldCapture: false, reason: "below_min_size" };
    }

    const ext = extractExtension(effectiveUrl, candidate.filename);
    const mime = normalizeMime(candidate.mime);
    if (!this.extensionSet.has(ext) && !this.mimeSet.has(mime)) {
      return { shouldCapture: false, reason: "unmatched_type" };
    }

    return { shouldCapture: true, reason: "matched" };
  }
}
