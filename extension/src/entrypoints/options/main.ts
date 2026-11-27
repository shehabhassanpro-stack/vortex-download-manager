import type { CaptureRules } from "@vortex/protocol";
import { DEFAULT_CAPTURE_RULES } from "../../lib/capture-engine";
import type { ExtensionLogEvent } from "../../lib/ring-buffer";

let loadedRules: CaptureRules = DEFAULT_CAPTURE_RULES;

const minSizeEl = document.getElementById("min-size-mb") as HTMLInputElement | null;
const extEl = document.getElementById("extensions-input") as HTMLTextAreaElement | null;
const hostsEl = document.getElementById("excluded-hosts") as HTMLTextAreaElement | null;
const incogEl = document.getElementById("incognito-toggle") as HTMLInputElement | null;
const formEl = document.getElementById("options-form") as HTMLFormElement | null;
const statusEl = document.getElementById("save-status");
const logsPreEl = document.getElementById("logs-view");
const refreshLogsBtn = document.getElementById("refresh-logs");

function populateForm(rules: CaptureRules): void {
  loadedRules = rules;
  if (minSizeEl !== null) {
    minSizeEl.value = String(Math.round(rules.minFileSizeBytes / (1024 * 1024)));
  }
  if (extEl !== null) {
    extEl.value = rules.extensions.join(", ");
  }
  if (hostsEl !== null) {
    hostsEl.value = rules.excludedHosts.join("\n");
  }
  if (incogEl !== null) {
    incogEl.checked = rules.captureInIncognito;
  }
}

function loadLogs(): void {
  void chrome.runtime.sendMessage({ kind: "vortex.getLogs" }).then((resp: unknown) => {
    if (typeof resp === "object" && resp !== null && "logs" in resp && logsPreEl !== null) {
      const logs = (resp as { readonly logs: readonly ExtensionLogEvent[] }).logs;
      logsPreEl.textContent =
        logs.length === 0
          ? "لا توجد أحداث مسجلة بعد."
          : logs.map((l) => JSON.stringify(l)).join("\n");
    }
  });
}

void chrome.runtime.sendMessage({ kind: "vortex.getStatus" }).then((resp: unknown) => {
  if (typeof resp === "object" && resp !== null && "rules" in resp) {
    populateForm((resp as { readonly rules: CaptureRules }).rules);
  }
});
loadLogs();

formEl?.addEventListener("submit", (ev) => {
  ev.preventDefault();
  const minMb = Math.max(0, Number(minSizeEl?.value ?? "1"));
  const extensions = (extEl?.value ?? "")
    .split(",")
    .map((s) => s.trim().toLowerCase().replace(/^\./, ""))
    .filter((s) => s.length > 0);
  const excludedHosts = (hostsEl?.value ?? "")
    .split(/\r?\n/)
    .map((s) => s.trim().toLowerCase())
    .filter((s) => s.length > 0);

  const updated: CaptureRules = {
    enabled: loadedRules.enabled,
    minFileSizeBytes: Math.round(minMb * 1024 * 1024),
    extensions,
    mimeTypes: loadedRules.mimeTypes,
    excludedHosts,
    captureInIncognito: incogEl?.checked ?? false,
  };

  void chrome.runtime.sendMessage({ kind: "vortex.updateRules", rules: updated }).then(() => {
    loadedRules = updated;
    if (statusEl !== null) {
      statusEl.textContent = "تم حفظ الإعدادات بنجاح";
    }
  });
});

refreshLogsBtn?.addEventListener("click", () => {
  loadLogs();
});
