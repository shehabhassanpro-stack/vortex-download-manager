import { existsSync } from "node:fs";
import { defineConfig } from "wxt";

/**
 * Deterministic 2048-bit RSA SPKI public key yielding fixed Chromium Extension ID:
 * `mpmpknhplomcjbdkbaopakoihjmhpbop`
 * Ensures `HKCU\...\NativeMessagingHosts\com.vortex.bridge` (`allowed_origins`) matches
 * unpacked dev builds (`just dev-ext-brave`) and Playwright E2E runs deterministically!
 */
export const VORTEX_EXTENSION_PUBLIC_KEY =
  "MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAh6vye4LEPuri3ONaAGCfc+6mL0senqHF32yWjpPayFRtFNAKqX/vAZLIhCbQjNso9kE1pjUpRP5F6hTRM9RO7GGLGZuIQmiYMKoQlEt9zSlLgPbriQXstgk4F+c0uyHx8MsiBSAOCdo4afqGgorvbp4ozB/63ocITebiRXTfQijmAwm/G1zR22JzsTLt6jsZB7oa9mYN5J+T98JvX0JZ9Omwg18I+HhiP/6cQW+KyK495N0/p6I1j22fqUBqKkh2ohZiF2h31do3l9nou2LjFyVuWdcpeWtzRr09TRMpf3LfR3K3YoH79cmzfRA6uaUUD4a87400RSUaxm74JHun6wIDAQAB";

const BRAVE_WIN_PATH = "C:\\Program Files\\BraveSoftware\\Brave-Browser\\Application\\brave.exe";

export default defineConfig({
  srcDir: "src",
  manifestVersion: 3,
  manifest: {
    name: "Vortex Download Manager",
    description:
      "Seamless high-speed download capture for Vortex Download Manager (Live-Stream Hold + CHIPS + Zero-Lost-Downloads).",
    version: "0.1.0",
    key: VORTEX_EXTENSION_PUBLIC_KEY,
    icons: {
      16: "icon/16.png",
      32: "icon/32.png",
      48: "icon/48.png",
      128: "icon/128.png",
    },
    action: {
      default_title: "Vortex Download Manager",
      default_icon: {
        16: "icon/16.png",
        32: "icon/32.png",
        48: "icon/48.png",
        128: "icon/128.png",
      },
    },
    permissions: [
      "activeTab",
      "downloads",
      "cookies",
      "contextMenus",
      "storage",
      "notifications",
      "nativeMessaging",
    ],
    host_permissions: ["http://*/*", "https://*/*"],
  },
  ...(existsSync(BRAVE_WIN_PATH)
    ? {
        webExt: {
          binaries: {
            chromium: BRAVE_WIN_PATH,
          },
        },
      }
    : {}),
});
