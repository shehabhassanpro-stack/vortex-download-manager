/* global Buffer, console, document, Image */
/**
 * Generates crisp antialiased multi-resolution PNG & Windows `.ico` icon assets
 * from `assets/vortex-logo.svg` using Playwright's Skia SVG rasterizer (`P1.5-T07`).
 *
 * Outputs:
 * - `apps/desktop/src-tauri/icons/{32x32.png, 128x128.png, 128x128@2x.png, icon.png, icon.ico}`
 * - `extension/public/icon/{16.png, 32.png, 48.png, 128.png}`
 */

import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { chromium } from "playwright-core";

const rootDir = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const svgPath = path.join(rootDir, "assets", "vortex-logo.svg");
const tauriIconsDir = path.join(rootDir, "apps", "desktop", "src-tauri", "icons");
const extIconsDir = path.join(rootDir, "extension", "public", "icon");

/**
 * Encodes an array of `{ size, png }` PNG buffers into a standard Vista+ PNG-compressed Windows `.ico`.
 */
function buildMultiResIco(entries) {
  const headerSize = 6;
  const dirEntrySize = 16;
  const count = entries.length;
  let offset = headerSize + count * dirEntrySize;

  const header = Buffer.alloc(headerSize);
  header.writeUInt16LE(0, 0); // Reserved
  header.writeUInt16LE(1, 2); // Type: 1 = ICO
  header.writeUInt16LE(count, 4);

  const dirBuffers = [];
  const dataBuffers = [];

  for (const { size, png } of entries) {
    const entry = Buffer.alloc(dirEntrySize);
    entry.writeUInt8(size >= 256 ? 0 : size, 0); // Width (0 means 256)
    entry.writeUInt8(size >= 256 ? 0 : size, 1); // Height (0 means 256)
    entry.writeUInt8(0, 2); // Color palette
    entry.writeUInt8(0, 3); // Reserved
    entry.writeUInt16LE(1, 4); // Color planes
    entry.writeUInt16LE(32, 6); // Bits per pixel
    entry.writeUInt32LE(png.length, 8); // Image byte length
    entry.writeUInt32LE(offset, 12); // Offset in file
    dirBuffers.push(entry);
    dataBuffers.push(png);
    offset += png.length;
  }

  return Buffer.concat([header, ...dirBuffers, ...dataBuffers]);
}

async function main() {
  const svgSource = fs.readFileSync(svgPath, "utf8");
  const svgDataUri = `data:image/svg+xml;base64,${Buffer.from(svgSource, "utf8").toString("base64")}`;

  const bravePath = "C:\\Program Files\\BraveSoftware\\Brave-Browser\\Application\\brave.exe";
  const launchOpts = fs.existsSync(bravePath)
    ? { executablePath: bravePath, headless: true }
    : { headless: true };

  const browser = await chromium.launch(launchOpts);
  try {
    const page = await browser.newPage({
      viewport: { width: 512, height: 512 },
      deviceScaleFactor: 1,
    });
    await page.setContent(
      `<!doctype html><html><body style="margin:0;background:transparent"></body></html>`,
    );

    const sizes = [16, 24, 32, 48, 64, 128, 256, 512];
    const pngBySize = new Map();

    for (const size of sizes) {
      const dataUrl = await page.evaluate(
        async ({ uri, targetSize }) => {
          const img = new Image();
          img.decoding = "sync";
          await new Promise((resolve, reject) => {
            img.onload = resolve;
            img.onerror = reject;
            img.src = uri;
          });
          const canvas = document.createElement("canvas");
          canvas.width = targetSize;
          canvas.height = targetSize;
          const ctx = canvas.getContext("2d");
          if (!ctx) throw new Error("2D canvas unavailable");
          ctx.imageSmoothingEnabled = true;
          ctx.imageSmoothingQuality = "high";
          ctx.clearRect(0, 0, targetSize, targetSize);
          ctx.drawImage(img, 0, 0, targetSize, targetSize);
          return canvas.toDataURL("image/png");
        },
        { uri: svgDataUri, targetSize: size },
      );

      const base64 = dataUrl.replace(/^data:image\/png;base64,/, "");
      pngBySize.set(size, Buffer.from(base64, "base64"));
    }

    fs.mkdirSync(tauriIconsDir, { recursive: true });
    fs.mkdirSync(extIconsDir, { recursive: true });

    // Tauri Desktop + Tray + Installer PNGs & multi-size `.ico`
    fs.writeFileSync(path.join(tauriIconsDir, "32x32.png"), pngBySize.get(32));
    fs.writeFileSync(path.join(tauriIconsDir, "128x128.png"), pngBySize.get(128));
    fs.writeFileSync(path.join(tauriIconsDir, "128x128@2x.png"), pngBySize.get(256));
    fs.writeFileSync(path.join(tauriIconsDir, "icon.png"), pngBySize.get(512));

    const icoBuf = buildMultiResIco([
      { size: 16, png: pngBySize.get(16) },
      { size: 24, png: pngBySize.get(24) },
      { size: 32, png: pngBySize.get(32) },
      { size: 48, png: pngBySize.get(48) },
      { size: 64, png: pngBySize.get(64) },
      { size: 128, png: pngBySize.get(128) },
      { size: 256, png: pngBySize.get(256) },
    ]);
    fs.writeFileSync(path.join(tauriIconsDir, "icon.ico"), icoBuf);

    // Brave / Chromium MV3 Extension crisp icons
    for (const s of [16, 32, 48, 128]) {
      fs.writeFileSync(path.join(extIconsDir, `${s}.png`), pngBySize.get(s));
    }

    console.log(
      `Generated Kinetic Vortex icons cleanly! icon.ico = ${(icoBuf.length / 1024).toFixed(1)} KiB`,
    );
  } finally {
    await browser.close();
  }
}

await main();
