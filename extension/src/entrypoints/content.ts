import { defineContentScript } from "wxt/utils/define-content-script";

/**
 * Lightweight trusted-event listener tracking `Alt+Click` bypass (`P1-T27` / `TH-05`).
 * Ignores untrusted synthetic page scripts (`!event.isTrusted`).
 */
export default defineContentScript({
  matches: ["http://*/*", "https://*/*"],
  runAt: "document_start",
  main() {
    window.addEventListener(
      "mousedown",
      (event: MouseEvent) => {
        if (event.isTrusted && event.altKey) {
          void chrome.runtime.sendMessage({ kind: "vortex.altKey" }).catch(() => {
            // Extension context reloaded
          });
        }
      },
      { capture: true, passive: true },
    );
  },
});
