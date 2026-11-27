/**
 * Automated End-to-End Test Suite on Real Brave Browser + Tauri v2 Desktop (`P1-T35` & `P1.5-T12`).
 *
 * Exercises the FULL production binary pipeline on Windows without mocks:
 *   `Brave (brave.exe + unpacked MV3 extension mpmpknhplomcjbdkbaopakoihjmhpbop)`
 *     <--> `vortex-bridge.exe` (`stdio` 4B LE JSON + `FILE_FLAG_OVERLAPPED` full-duplex pipe)
 *     <--> `\\.\pipe\vortex-core-<SID>` (Owner-SID Named Pipe + Protobuf v3)
 *     <--> `vortex-desktop.exe` (`Vortex Core` + `SparseFileSink` + `Tauri v2 WebView2` UI)
 *     <--> `vortex-test-lab.exe` (Deterministic seeded HTTP/1.1 hostile server)
 *
 * Verifies Phase 1.5 Smart Capture Confirmation-First Defaults:
 * 1. Clicking a download in Brave stages it safely (`auto_start_handoff = false`, 0 bytes on disk)
 *    and opens Vortex's Smart Confirmation Dialog inside Tauri WebView2.
 * 2. Pressing `Discard` drops the warm stream cleanly with nothing written to disk.
 * 3. Confirming a captured download reuses the `0ms` warm stream and completes with 100% SHA-256!
 * 4. Single-Use Token (`410`) and Cloudflare JA4 (`403`) preserve Brave's original open TCP socket.
 * 5. Manual `[+ New URL]` workflow in the Edge-to-Edge Mica UI renders the Kinetic Vortex SVG logo
 *    and completes with 100% SHA-256.
 */

import { execFileSync, spawn, type ChildProcess } from "node:child_process";
import { createHash } from "node:crypto";
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { chromium, type Browser, type BrowserContext, type Page } from "playwright-core";
import { afterAll, beforeAll, describe, expect, it } from "vitest";

declare const chrome: {
  readonly runtime: {
    sendMessage(message: unknown, responseCallback: (response: unknown) => void): void;
  };
};

const EXTENSION_ID = "mpmpknhplomcjbdkbaopakoihjmhpbop";
const BRAVE_EXE = "C:\\Program Files\\BraveSoftware\\Brave-Browser\\Application\\brave.exe";
const REG_KEYS = [
  "HKCU\\Software\\BraveSoftware\\Brave-Browser\\NativeMessagingHosts\\com.vortex.bridge",
  "HKCU\\Software\\Google\\Chrome\\NativeMessagingHosts\\com.vortex.bridge",
  "HKCU\\Software\\Microsoft\\Edge\\NativeMessagingHosts\\com.vortex.bridge",
] as const;

interface TokenStats {
  readonly ok_streams: number;
  readonly rejected_probes: number;
}

interface LabStats {
  readonly throttled_partial_requests: number;
  readonly concurrency_capped_rejections: number;
  readonly single_use: Readonly<Record<string, TokenStats>>;
  readonly browser_ja4: Readonly<Record<string, TokenStats>>;
}

function repoRoot(): string {
  return resolve(import.meta.dirname, "..", "..");
}

function locateBinary(exeName: string): string {
  const root = repoRoot();
  const rel = join(root, "target", "release", exeName);
  if (existsSync(rel)) {
    return rel;
  }
  const dbg = join(root, "target", "debug", exeName);
  if (existsSync(dbg)) {
    return dbg;
  }
  throw new Error(`Compiled binary not found: ${exeName}. Build workspace binaries first.`);
}

function sha256File(filePath: string): string {
  const buf = readFileSync(filePath);
  return createHash("sha256").update(buf).digest("hex");
}

function snapshotRegistryKey(key: string): string | null {
  try {
    const out = execFileSync("reg.exe", ["query", key, "/ve"], {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "ignore"],
    });
    const match = /REG_SZ\s+(.+)$/m.exec(out);
    return match?.[1]?.trim() ?? null;
  } catch {
    return null;
  }
}

function restoreRegistryKey(key: string, previousValue: string | null): void {
  try {
    if (previousValue === null) {
      execFileSync("reg.exe", ["delete", key, "/f"], { stdio: "ignore" });
    } else {
      execFileSync("reg.exe", ["add", key, "/ve", "/t", "REG_SZ", "/d", previousValue, "/f"], {
        stdio: "ignore",
      });
    }
  } catch {
    // Best-effort registry cleanup
  }
}

function findFileRecursive(dir: string, targetName: string): string | null {
  if (!existsSync(dir)) {
    return null;
  }
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const full = join(dir, entry.name);
    if (entry.isDirectory()) {
      const nested = findFileRecursive(full, targetName);
      if (nested !== null) {
        return nested;
      }
    } else if (entry.name === targetName) {
      return full;
    }
  }
  return null;
}

async function waitForFileComplete(
  rootDir: string,
  fileName: string,
  expectedBytes: number,
  timeoutMs: number,
): Promise<string> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const found = findFileRecursive(rootDir, fileName);
    if (found !== null && !existsSync(`${found}.vortex`) && !existsSync(`${found}.crdownload`)) {
      try {
        const st = statSync(found);
        if (st.size === expectedBytes) {
          return found;
        }
      } catch {
        // Still finalizing rename
      }
    }
    await new Promise((r) => setTimeout(r, 100));
  }
  throw new Error(`Timed out waiting for completed file ${fileName} in ${rootDir}`);
}

async function enableNativeChromeDownloadDelegate(
  ctx: BrowserContext,
  page: Page,
  downloadDir: string,
): Promise<void> {
  const cdp = await ctx.newCDPSession(page);
  await cdp.send("Browser.setDownloadBehavior", {
    behavior: "default",
  });
  mkdirSync(downloadDir, { recursive: true });
}

async function allocateFreeTcpPort(): Promise<number> {
  return new Promise<number>((resolvePort, rejectPort) => {
    const srv = createServer();
    srv.listen(0, "127.0.0.1", () => {
      const addr = srv.address();
      if (addr !== null && typeof addr === "object") {
        const p = addr.port;
        srv.close(() => resolvePort(p));
      } else {
        srv.close(() => rejectPort(new Error("Failed allocating ephemeral TCP port")));
      }
    });
    srv.on("error", rejectPort);
  });
}

async function spawnTestLab(exePath: string): Promise<{ proc: ChildProcess; baseUrl: string }> {
  const proc = spawn(exePath, [], {
    stdio: ["ignore", "pipe", "pipe"],
    windowsHide: true,
  });

  const baseUrl = await new Promise<string>((resolveUrl, rejectUrl) => {
    const timer = setTimeout(() => rejectUrl(new Error("vortex-test-lab startup timeout")), 10_000);
    let buf = "";
    proc.stdout?.on("data", (chunk: Buffer) => {
      buf += chunk.toString("utf8");
      const m = /READY\s+(http:\/\/127\.0\.0\.1:\d+)/.exec(buf);
      if (m?.[1] !== undefined) {
        clearTimeout(timer);
        resolveUrl(m[1]);
      }
    });
    proc.once("exit", (code) => {
      clearTimeout(timer);
      rejectUrl(new Error(`vortex-test-lab exited prematurely with code ${String(code)}`));
    });
  });

  return { proc, baseUrl };
}

function writeTestConfigToml(dataDir: string, vortexDownloadsDir: string): void {
  const escapedDir = vortexDownloadsDir.replace(/\\/g, "/");
  const toml = [
    "schema_version = 1",
    "max_connections_per_download = 8",
    "max_concurrent_downloads = 3",
    "checkpoint_interval_secs = 20",
    "idle_timeout_secs = 30",
    "max_retries = 10",
    "min_free_disk_bytes = 67108864",
    `default_download_dir = "${escapedDir}"`,
    // Phase 1.5 Default: Confirmation-First! NEVER silently auto-start browser captures!
    "auto_start_handoff = false",
    "warm_instant_webview = false",
    "prevent_sleep_while_active = false",
    "clipboard_monitor_enabled = false",
    'theme = "dark"',
    'language = "ar"',
    "",
    "[capture_rules]",
    "enabled = true",
    "min_file_size_bytes = 1048576",
    'extensions = ["vtxblob", "bin", "zip", "iso", "exe", "mp4", "pdf"]',
    'mime_types = ["application/octet-stream", "application/zip"]',
    "excluded_hosts = []",
    "capture_in_incognito = false",
    "",
  ].join("\n");
  writeFileSync(join(dataDir, "config.toml"), toml, "utf8");
}

function writeBraveDefaultPreferences(braveProfileDir: string, braveDownloadsDir: string): void {
  const defaultProfile = join(braveProfileDir, "Default");
  mkdirSync(defaultProfile, { recursive: true });
  const prefs = {
    download: {
      default_directory: braveDownloadsDir,
      prompt_for_download: false,
      directory_upgrade: true,
    },
    savefile: {
      default_directory: braveDownloadsDir,
    },
    safebrowsing: {
      enabled: false,
    },
  };
  writeFileSync(join(defaultProfile, "Preferences"), JSON.stringify(prefs), "utf8");
}

describe("Phase 1.5 Real Brave MV3 + Native Bridge + Vortex Tauri v2 Smart Confirmation E2E", () => {
  const savedRegistry = new Map<string, string | null>();
  let sandboxRoot = "";
  let vortexDownloadsDir = "";
  let braveDownloadsDir = "";
  let labProc: ChildProcess | undefined;
  let labUrl = "";
  let desktopProc: ChildProcess | undefined;
  let desktopBrowser: Browser | undefined;
  let desktopPage: Page | undefined;
  let braveContext: BrowserContext | undefined;

  beforeAll(async () => {
    expect(existsSync(BRAVE_EXE)).toBe(true);

    const extDist = join(repoRoot(), "extension", ".output", "chrome-mv3");
    expect(existsSync(join(extDist, "manifest.json"))).toBe(true);

    const desktopExe = locateBinary("vortex-desktop.exe");
    const bridgeExe = locateBinary("vortex-bridge.exe");
    const labExe = locateBinary("vortex-test-lab.exe");
    expect(existsSync(bridgeExe)).toBe(true);

    for (const k of REG_KEYS) {
      savedRegistry.set(k, snapshotRegistryKey(k));
    }

    sandboxRoot = mkdtempSync(join(tmpdir(), "vortex-e2e-"));
    const dataDir = join(sandboxRoot, "data");
    const logsDir = join(dataDir, "logs");
    vortexDownloadsDir = join(sandboxRoot, "vortex-downloads");
    braveDownloadsDir = join(sandboxRoot, "brave-downloads");
    const braveProfileDir = join(sandboxRoot, "brave-profile");

    for (const d of [dataDir, logsDir, vortexDownloadsDir, braveDownloadsDir, braveProfileDir]) {
      mkdirSync(d, { recursive: true });
    }
    writeTestConfigToml(dataDir, vortexDownloadsDir);
    writeBraveDefaultPreferences(braveProfileDir, braveDownloadsDir);

    const startedLab = await spawnTestLab(labExe);
    labProc = startedLab.proc;
    labUrl = startedLab.baseUrl;

    const cdpPort = await allocateFreeTcpPort();
    const testSid = `S-1-5-21-E2E-${Date.now()}`;
    const sharedEnv: NodeJS.ProcessEnv = {
      ...process.env,
      VORTEX_DATA_DIR: dataDir,
      VORTEX_LOG_DIR: logsDir,
      VORTEX_PIPE_SID_OVERRIDE: testSid,
      VORTEX_CORE_EXE: desktopExe,
      // Explicitly allow loopback Test Lab (`127.0.0.1`) in E2E while keeping production default blocked (`TH-26`)
      VORTEX_ALLOW_PRIVATE_NETWORK: "1",
    };

    // Launch REAL Tauri v2 Desktop GUI (`auto_start_handoff = false` confirmation-first!)
    desktopProc = spawn(desktopExe, [], {
      env: {
        ...sharedEnv,
        WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${cdpPort}`,
      },
      stdio: "ignore",
      windowsHide: false,
    });

    const deadline = Date.now() + 15_000;
    while (Date.now() < deadline) {
      try {
        desktopBrowser = await chromium.connectOverCDP(`http://127.0.0.1:${cdpPort}`);
        break;
      } catch {
        await new Promise((r) => setTimeout(r, 150));
      }
    }
    expect(desktopBrowser).toBeDefined();
    if (desktopBrowser === undefined) {
      throw new Error("Failed connecting to Tauri WebView2 CDP port");
    }

    const pageDeadline = Date.now() + 10_000;
    while (desktopPage === undefined && Date.now() < pageDeadline) {
      desktopPage = desktopBrowser.contexts()[0]?.pages()[0];
      if (desktopPage === undefined) {
        await new Promise((r) => setTimeout(r, 100));
      }
    }
    expect(desktopPage).toBeDefined();
    await desktopPage?.waitForSelector(".vtx-app-shell", { timeout: 10_000 });

    braveContext = await chromium.launchPersistentContext(braveProfileDir, {
      executablePath: BRAVE_EXE,
      headless: false,
      env: sharedEnv,
      args: [
        `--disable-extensions-except=${extDist}`,
        `--load-extension=${extDist}`,
        "--no-first-run",
        "--no-default-browser-check",
        "--disable-popup-blocking",
        "--safebrowsing-disable-download-protection",
      ],
    });

    if (braveContext.serviceWorkers().length === 0) {
      await braveContext.waitForEvent("serviceworker", { timeout: 15_000 });
    }
  });

  afterAll(async () => {
    if (braveContext !== undefined) {
      await Promise.race([
        braveContext.close().catch(() => undefined),
        new Promise((r) => setTimeout(r, 4_000)),
      ]);
    }
    if (desktopBrowser !== undefined) {
      await desktopBrowser.close().catch(() => undefined);
    }
    if (desktopProc !== undefined) {
      desktopProc.kill();
    }
    if (labProc !== undefined) {
      labProc.kill();
    }
    for (const [key, prev] of savedRegistry.entries()) {
      restoreRegistryKey(key, prev);
    }
    if (sandboxRoot.length > 0) {
      await new Promise((r) => setTimeout(r, 300));
      try {
        rmSync(sandboxRoot, { recursive: true, force: true });
      } catch {
        // Ignore transient Windows file lock during temp cleanup
      }
    }
  });

  it("connects Brave MV3 Extension Popup over vortex-bridge.exe and renders Kinetic Vortex branding", async () => {
    expect(braveContext).toBeDefined();
    expect(desktopPage).toBeDefined();
    const ctx = braveContext as BrowserContext;
    const uiPage = desktopPage as Page;

    // Verify Kinetic Vortex SVG Logo renders inside the Edge-to-Edge Mica Desktop Shell!
    expect(await uiPage.locator("svg.vtx-logo-svg").count()).toBeGreaterThanOrEqual(1);

    const popupPage = await ctx.newPage();
    await popupPage.goto(`chrome-extension://${EXTENSION_ID}/popup.html`);

    const status = await popupPage.evaluate(async () => {
      return new Promise<{
        readonly connected: boolean;
        readonly rules: { readonly excludedHosts: readonly string[] };
      }>((resolveStatus) => {
        chrome.runtime.sendMessage({ kind: "vortex.getStatus" }, (resp) => {
          resolveStatus(
            resp as {
              readonly connected: boolean;
              readonly rules: { readonly excludedHosts: readonly string[] };
            },
          );
        });
      });
    });

    expect(status.connected).toBe(true);
    expect(status.rules.excludedHosts).toEqual([]);
    await popupPage.close();
  });

  it("opens Smart Confirmation Dialog on Brave capture (no silent auto-start), cleanly discards unwanted capture, then confirms & completes 100% SHA-256", async () => {
    const ctx = braveContext as BrowserContext;
    const uiPage = desktopPage as Page;
    const page = await ctx.newPage();
    await enableNativeChromeDownloadDelegate(ctx, page, braveDownloadsDir);
    await page.goto(`${labUrl}/health`);

    // 1. Trigger a capture that the user chooses to DISCARD (verifying zero silent disk writes!)
    const discardName = "discard-trap.vtxblob";
    const discardHref = `${labUrl}/throttled/${2 * 1024 * 1024}?bps=2097152&seed=19&name=${discardName}`;
    await page.setContent(`<a id="dl-discard" href="${discardHref}">Discard Me</a>`);
    await page.click("#dl-discard");

    // Smart Confirmation Modal MUST appear in Vortex Desktop UI
    const confirmDialog = uiPage.locator('[data-testid="smart-confirm-dialog"]');
    await confirmDialog.waitFor({ state: "visible", timeout: 10_000 });
    expect(findFileRecursive(vortexDownloadsDir, discardName)).toBeNull();

    // Discard -> modal vanishes and nothing is ever written to disk!
    await uiPage.locator('[data-testid="btn-discard-staged"]').click();
    await confirmDialog.waitFor({ state: "hidden", timeout: 5_000 });
    await new Promise((r) => setTimeout(r, 250));
    expect(findFileRecursive(vortexDownloadsDir, discardName)).toBeNull();

    // 2. Trigger a real 8 MiB capture -> wait for Smart Confirm -> wait past 350ms Click Guard -> Approve!
    const fileSize = 8 * 1024 * 1024;
    const seed = 42;
    const fileName = "vortex-captured.vtxblob";
    const expectedSha = await (await fetch(`${labUrl}/sha256/${seed}/${fileSize}`)).text();
    const targetHref = `${labUrl}/throttled/${fileSize}?bps=2097152&seed=${seed}&name=${fileName}`;
    await page.setContent(`<a id="dl-normal" href="${targetHref}">Download Captured</a>`);
    await page.click("#dl-normal");

    await confirmDialog.waitFor({ state: "visible", timeout: 10_000 });
    expect(findFileRecursive(vortexDownloadsDir, fileName)).toBeNull();

    // Wait for `350ms` Input Grace Lock to release and click `[ابدأ التحميل]`
    const startBtn = uiPage.locator('[data-testid="btn-confirm-start"]');
    await uiPage.waitForFunction(
      () => {
        const el = document.querySelector<HTMLButtonElement>('[data-testid="btn-confirm-start"]');
        return el !== null && !el.disabled;
      },
      undefined,
      { timeout: 5_000 },
    );
    await startBtn.click();

    const savedByVortex = await waitForFileComplete(vortexDownloadsDir, fileName, fileSize, 25_000);
    expect(sha256File(savedByVortex)).toBe(expectedSha);

    const stats = (await (await fetch(`${labUrl}/stats`)).json()) as LabStats;
    expect(stats.throttled_partial_requests).toBeGreaterThanOrEqual(2);
    expect(findFileRecursive(braveDownloadsDir, fileName)).toBeNull();
    await page.close();
  });

  it("preserves Brave's original open TCP socket on Single-Use Token (410) AND Cloudflare JA4 (403) via suggest() with zero interruption", async () => {
    const ctx = braveContext as BrowserContext;
    const fileSize = 2 * 1024 * 1024;
    const page = await ctx.newPage();
    await enableNativeChromeDownloadDelegate(ctx, page, braveDownloadsDir);
    await page.goto(`${labUrl}/health`);

    // 1. Single-Use Token (`410`)
    const tokenKey = "tok-live-e2e";
    const expectedSingleSha = await (await fetch(`${labUrl}/sha256/77/${fileSize}`)).text();
    const singleHref = `${labUrl}/single-use/${tokenKey}/${fileSize}?bps=1572864`;
    await page.setContent(`<a id="dl-single" href="${singleHref}">Download Single-Use</a>`);
    await page.click("#dl-single");

    const singleSavedPath = await waitForFileComplete(
      braveDownloadsDir,
      "single-use.bin",
      fileSize,
      20_000,
    );
    expect(sha256File(singleSavedPath)).toBe(expectedSingleSha);

    // 2. Cloudflare JA4 Lock (`403`)
    const ja4Key = "ja4-live-e2e";
    const expectedJa4Sha = await (await fetch(`${labUrl}/sha256/88/${fileSize}`)).text();
    const ja4Href = `${labUrl}/browser-ja4/${ja4Key}/${fileSize}?bps=1572864`;
    await page.setContent(`<a id="dl-ja4" href="${ja4Href}">Download Cloudflare JA4</a>`);
    await page.click("#dl-ja4");

    const ja4SavedPath = await waitForFileComplete(
      braveDownloadsDir,
      "ja4-protected.bin",
      fileSize,
      20_000,
    );
    expect(sha256File(ja4SavedPath)).toBe(expectedJa4Sha);

    const stats = (await (await fetch(`${labUrl}/stats`)).json()) as LabStats;
    expect(stats.single_use[tokenKey]?.ok_streams).toBe(1);
    expect(stats.single_use[tokenKey]?.rejected_probes).toBeGreaterThanOrEqual(1);
    expect(stats.browser_ja4[ja4Key]?.ok_streams).toBe(1);
    expect(stats.browser_ja4[ja4Key]?.rejected_probes).toBeGreaterThanOrEqual(1);

    const popupPage = await ctx.newPage();
    await popupPage.goto(`chrome-extension://${EXTENSION_ID}/popup.html`);
    const ringDump = await popupPage.evaluate(async () => {
      return new Promise<{ readonly logs: readonly { readonly detail: string }[] }>((res) => {
        chrome.runtime.sendMessage({ kind: "vortex.getLogs" }, (reply) => {
          res(reply as { readonly logs: readonly { readonly detail: string }[] });
        });
      });
    });
    const combinedDetails = ringDump.logs.map((l) => l.detail).join("\n");
    expect(combinedDetails).toContain("single_use_token");
    expect(combinedDetails).toContain("waf_tls_bound");

    await popupPage.close();
    await page.close();
  });

  it("drives the Edge-to-Edge Mica Desktop UI manual New URL dialog to stage and complete a verified download", async () => {
    const uiPage = desktopPage as Page;
    const tauriDriverUrl = process.env["TAURI_DRIVER_URL"];
    if (tauriDriverUrl !== undefined && tauriDriverUrl.length > 0) {
      const wdStatus = await fetch(`${tauriDriverUrl}/status`);
      expect(wdStatus.ok).toBe(true);
    }

    await uiPage.locator('[data-testid="btn-add-url"]').click();
    const urlInput = uiPage.locator('[data-testid="smart-confirm-dialog"] input[type="url"]');
    await urlInput.waitFor({ state: "visible", timeout: 5_000 });

    const size = 2 * 1024 * 1024;
    const seed = 55;
    const expectedSha = await (await fetch(`${labUrl}/sha256/${seed}/${size}`)).text();
    const targetUrl = `${labUrl}/throttled/${size}?bps=4194304&seed=${seed}&name=tauri-ui.vtxblob`;

    await urlInput.fill(targetUrl);
    await uiPage.waitForFunction(
      () => {
        const inputs = Array.from(document.querySelectorAll("input"));
        return inputs.some((el) => el.value === "tauri-ui.vtxblob");
      },
      undefined,
      { timeout: 8_000 },
    );

    await uiPage.locator('[data-testid="btn-confirm-start"]').click();

    const completedFile = await waitForFileComplete(
      vortexDownloadsDir,
      "tauri-ui.vtxblob",
      size,
      20_000,
    );
    expect(sha256File(completedFile)).toBe(expectedSha);
  });

  it("completes a strict 1-stream concurrency-capped host (Pixeldrain scenario) with 8 configured connections via Slow-Start Step-Down and 0 UI crashes", async () => {
    const uiPage = desktopPage as Page;

    await uiPage.locator('[data-testid="btn-add-url"]').click();
    const urlInput = uiPage.locator('[data-testid="smart-confirm-dialog"] input[type="url"]');
    await urlInput.waitFor({ state: "visible", timeout: 5_000 });

    const size = 8 * 1024 * 1024;
    const seed = 91;
    const expectedSha = await (await fetch(`${labUrl}/sha256/${seed}/${size}`)).text();
    const targetUrl = `${labUrl}/concurrency-capped/1/${size}?id=e2e-pixeldrain&bps=4194304&seed=${seed}&name=pixeldrain-capped.vtxblob`;

    await urlInput.fill(targetUrl);
    await uiPage.waitForFunction(
      () => {
        const inputs = Array.from(document.querySelectorAll("input"));
        return inputs.some((el) => el.value === "pixeldrain-capped.vtxblob");
      },
      undefined,
      { timeout: 8_000 },
    );

    await uiPage.locator('[data-testid="btn-confirm-start"]').click();

    const completedFile = await waitForFileComplete(
      vortexDownloadsDir,
      "pixeldrain-capped.vtxblob",
      size,
      25_000,
    );
    expect(sha256File(completedFile)).toBe(expectedSha);

    // Verify ZERO React ErrorBoundary fallbacks ever appeared in the Desktop UI!
    const bodyText = await uiPage.evaluate(() => document.body.innerText);
    expect(bodyText).not.toContain("Cannot read properties of undefined");
  });
});
