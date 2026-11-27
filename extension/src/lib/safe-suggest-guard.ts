/**
 * `SafeSuggestGuard` (`P1-T28` / `Protocol-Spec.md` §4 — Zero-Lost-Download Guarantee).
 *
 * Holds Chromium's `onDeterminingFilename` `suggest()` callback WITHOUT EVER calling
 * `chrome.downloads.pause()`, so the browser's live TCP socket and single-use tokens remain
 * 100% open. Guarantees `suggest()` is invoked at most once and ALWAYS fires on timeout (`2.0s`)
 * or unexpected error unless Vortex explicitly took ownership and cancelled the browser item.
 */

import { SAFE_SUGGEST_GUARD_TIMEOUT_MS } from "@vortex/protocol";

export type SuggestFn = (suggestion?: chrome.downloads.DownloadFilenameSuggestion) => void;

export class SafeSuggestGuard {
  private settled = false;
  private timerId: ReturnType<typeof setTimeout> | undefined;
  private readonly suggestFn: SuggestFn;
  private readonly onTimeoutFallback: (() => void) | undefined;

  public constructor(
    suggestFn: SuggestFn,
    timeoutMs: number = SAFE_SUGGEST_GUARD_TIMEOUT_MS,
    onTimeoutFallback?: () => void,
  ) {
    this.suggestFn = suggestFn;
    this.onTimeoutFallback = onTimeoutFallback;
    this.timerId = setTimeout(() => {
      if (!this.settled) {
        this.onTimeoutFallback?.();
        this.releaseToBrowser();
      }
    }, timeoutMs);
  }

  public isSettled(): boolean {
    return this.settled;
  }

  /**
   * Releases `onDeterminingFilename` via `suggest()` so Brave/Chromium seamlessly finishes
   * downloading on its **original open TCP connection** (`0` extra HTTP requests!).
   */
  public releaseToBrowser(): boolean {
    if (this.settled) {
      return false;
    }
    this.settled = true;
    this.clearTimer();
    try {
      this.suggestFn();
    } catch (err) {
      console.warn("[Vortex] suggest() threw after tab/download closed:", err);
    }
    return true;
  }

  /**
   * Marks that Vortex Core accepted the transfer (`probe_and_stream` succeeded) and the extension
   * is cancelling + erasing the browser's duplicate item.
   */
  public markTakenOverByVortex(): boolean {
    if (this.settled) {
      return false;
    }
    this.settled = true;
    this.clearTimer();
    return true;
  }

  private clearTimer(): void {
    if (this.timerId !== undefined) {
      clearTimeout(this.timerId);
      this.timerId = undefined;
    }
  }
}
