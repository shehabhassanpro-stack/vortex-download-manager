import { describe, expect, it } from "vitest";
import {
  PROTOCOL_VERSION,
  SAFE_SUGGEST_GUARD_TIMEOUT_MS,
  isAllowedTransferUrl,
  type Envelope,
} from "./index.js";

describe("@vortex/protocol contract", () => {
  it("enforces http/https allowlist and rejects UNC / file / javascript schemes", () => {
    expect(isAllowedTransferUrl("https://cdn.example.com/archive.zip")).toBe(true);
    expect(isAllowedTransferUrl("http://localhost:8080/file.bin")).toBe(true);
    expect(isAllowedTransferUrl("\\\\attacker\\share\\evil.exe")).toBe(false);
    expect(isAllowedTransferUrl("file:///C:/Windows/win.ini")).toBe(false);
    expect(isAllowedTransferUrl("javascript:alert(1)")).toBe(false);
    expect(isAllowedTransferUrl("data:text/plain;base64,QQ==")).toBe(false);
  });

  it("constructs a type-safe Channel A DownloadHandoff envelope", () => {
    const env: Envelope<"download.handoff"> = {
      v: PROTOCOL_VERSION,
      id: "0192f3a4-0000-7000-8000-000000000010",
      replyTo: "",
      traceId: "0192f3a4-0000-7000-8000-000000000001",
      timestampMs: 1764094000000,
      type: "download.handoff",
      payload: {
        url: "https://example.com/file.zip",
        finalUrl: "https://cdn.example.com/file.zip",
        referrer: "https://example.com/",
        topLevelSite: "https://example.com",
        suggestedName: "file.zip",
        mimeType: "application/zip",
        sizeBytes: 1048576,
        userAgent: "Mozilla/5.0",
        cookies: "",
        headers: { "Accept-Language": "ar,en;q=0.9" },
        incognito: false,
        source: "determining-filename",
        autoStart: false,
      },
    };

    expect(env.v).toBe(1);
    expect(SAFE_SUGGEST_GUARD_TIMEOUT_MS).toBe(2000);
    expect(env.payload.topLevelSite).toBe("https://example.com");
  });
});
