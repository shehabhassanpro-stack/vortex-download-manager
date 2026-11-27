/**
 * Standard + CHIPS (`Cookies Having Independent Partitioned State`) Cookie Merger (`P1-T28`).
 *
 * Queries both unpartitioned cookies (`chrome.cookies.getAll({ url })`) and CHIPS partitioned
 * cookies (`chrome.cookies.getAll({ url, partitionKey: { topLevelSite } })`), deduplicating
 * safely within the `64 KiB` protocol budget and NEVER logging cookie values (`TH-08`).
 */

import { MAX_COOKIES_BYTES } from "@vortex/protocol";

export interface CookiesApiLike {
  getAll(details: chrome.cookies.GetAllDetails): Promise<chrome.cookies.Cookie[]>;
}

/**
 * Derives the canonical CHIPS `topLevelSite` (`scheme://registrable-host`) from a referrer or tab URL.
 */
export function deriveTopLevelSite(referrerOrTabUrl?: string): string {
  if (referrerOrTabUrl === undefined || referrerOrTabUrl.trim().length === 0) {
    return "";
  }
  try {
    const parsed = new URL(referrerOrTabUrl);
    if (parsed.protocol !== "http:" && parsed.protocol !== "https:") {
      return "";
    }
    return `${parsed.protocol}//${parsed.hostname}`;
  } catch {
    return "";
  }
}

/**
 * Merges normal + CHIPS partitioned cookies for `downloadUrl` into a single `Cookie` header value.
 */
export async function collectMergedCookies(
  cookiesApi: CookiesApiLike,
  downloadUrl: string,
  topLevelSite: string,
): Promise<string> {
  const byName = new Map<string, string>();

  try {
    const normal = await cookiesApi.getAll({ url: downloadUrl });
    for (const c of normal) {
      if (c.name.length > 0 && !c.name.includes("\r") && !c.value.includes("\r")) {
        byName.set(c.name, c.value);
      }
    }
  } catch {
    // Host permission or incognito cookie store unavailable — continue gracefully
  }

  if (topLevelSite.length > 0) {
    try {
      const partitioned = await cookiesApi.getAll({
        url: downloadUrl,
        partitionKey: { topLevelSite },
      });
      for (const c of partitioned) {
        if (c.name.length > 0 && !c.name.includes("\r") && !c.value.includes("\r")) {
          // Partitioned CHIPS cookies take precedence for embedded CDN frames!
          byName.set(c.name, c.value);
        }
      }
    } catch {
      // Older Chromium builds without `partitionKey` ignore cleanly
    }
  }

  if (byName.size === 0) {
    return "";
  }

  const pairs: string[] = [];
  let byteCount = 0;
  const encoder = new TextEncoder();

  for (const [name, value] of byName.entries()) {
    const pair = `${name}=${value}`;
    const pairBytes = encoder.encode(pair).byteLength + (pairs.length > 0 ? 2 : 0);
    if (byteCount + pairBytes > MAX_COOKIES_BYTES) {
      break;
    }
    pairs.push(pair);
    byteCount += pairBytes;
  }

  return pairs.join("; ");
}
