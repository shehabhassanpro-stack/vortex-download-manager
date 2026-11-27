import type { CaptureRules } from "@vortex/protocol";
import { DEFAULT_CAPTURE_RULES } from "../../lib/capture-engine";

interface StatusReply {
  readonly connected: boolean;
  readonly rules: CaptureRules;
}

interface QuickReply {
  readonly ok: boolean;
  readonly error?: string;
}

let currentRules: CaptureRules = DEFAULT_CAPTURE_RULES;
let activeTabHost: string | null = null;

const badgeEl = document.getElementById("status-badge");
const statusTextEl = document.getElementById("status-text");
const toggleEl = document.getElementById("capture-toggle") as HTMLInputElement | null;
const siteCardEl = document.getElementById("site-card");
const currentHostEl = document.getElementById("current-host");
const toggleSiteBtn = document.getElementById("toggle-site-btn") as HTMLButtonElement | null;
const formEl = document.getElementById("quick-form") as HTMLFormElement | null;
const urlInputEl = document.getElementById("quick-url") as HTMLInputElement | null;
const feedbackEl = document.getElementById("feedback-msg");
const optionsBtnEl = document.getElementById("open-options");

function isHostExcluded(host: string, rules: CaptureRules): boolean {
  const norm = host.toLowerCase();
  return rules.excludedHosts.some((h) => h.toLowerCase() === norm);
}

function renderSiteToggle(): void {
  if (siteCardEl === null || currentHostEl === null || toggleSiteBtn === null) {
    return;
  }
  if (activeTabHost === null) {
    siteCardEl.hidden = true;
    return;
  }
  siteCardEl.hidden = false;
  currentHostEl.textContent = activeTabHost;
  const excluded = isHostExcluded(activeTabHost, currentRules);
  toggleSiteBtn.dataset["excluded"] = excluded ? "true" : "false";
  toggleSiteBtn.textContent = excluded ? "إعادة تفعيل الموقع" : "استثناء هذا الموقع";
}

function renderStatus(status: StatusReply): void {
  currentRules = status.rules;
  if (badgeEl !== null) {
    badgeEl.dataset["connected"] = status.connected ? "true" : "false";
  }
  if (statusTextEl !== null) {
    statusTextEl.textContent = status.connected ? "متصل بـ Vortex" : "وضع المتصفح الاحتياطي";
  }
  if (toggleEl !== null) {
    toggleEl.checked = status.rules.enabled;
  }
  renderSiteToggle();
}

void chrome.runtime
  .sendMessage({ kind: "vortex.getStatus" })
  .then((resp: unknown) => {
    if (typeof resp === "object" && resp !== null && "connected" in resp) {
      renderStatus(resp as StatusReply);
    }
  })
  .catch(() => {
    renderStatus({ connected: false, rules: DEFAULT_CAPTURE_RULES });
  });

// Inspect active browser tab hostname for 1-click site exclusion (`activeTab` permission — `P1.5-T11`)
if (typeof chrome.tabs !== "undefined" && typeof chrome.tabs.query === "function") {
  void chrome.tabs
    .query({ active: true, currentWindow: true })
    .then((tabs) => {
      const urlStr = tabs[0]?.url;
      if (urlStr === undefined) return;
      try {
        const parsed = new URL(urlStr);
        if (parsed.protocol === "http:" || parsed.protocol === "https:") {
          activeTabHost = parsed.hostname.toLowerCase();
          renderSiteToggle();
        }
      } catch {
        // Non-HTTP browser internal tab
      }
    })
    .catch(() => undefined);
}

toggleEl?.addEventListener("change", () => {
  const nextRules: CaptureRules = {
    ...currentRules,
    enabled: toggleEl.checked,
  };
  currentRules = nextRules;
  void chrome.runtime.sendMessage({ kind: "vortex.updateRules", rules: nextRules });
});

toggleSiteBtn?.addEventListener("click", () => {
  if (activeTabHost === null) return;
  const excluded = isHostExcluded(activeTabHost, currentRules);
  const nextExcluded = excluded
    ? currentRules.excludedHosts.filter((h) => h.toLowerCase() !== activeTabHost)
    : [...currentRules.excludedHosts, activeTabHost];

  const nextRules: CaptureRules = {
    ...currentRules,
    excludedHosts: nextExcluded,
  };
  currentRules = nextRules;
  renderSiteToggle();
  void chrome.runtime.sendMessage({ kind: "vortex.updateRules", rules: nextRules });
  if (feedbackEl !== null) {
    feedbackEl.textContent = excluded
      ? `تم إعادة تفعيل الالتقاط على ${activeTabHost}`
      : `تم استثناء ${activeTabHost} من الالتقاط التلقائي`;
  }
});

formEl?.addEventListener("submit", (ev) => {
  ev.preventDefault();
  const rawUrl = urlInputEl?.value.trim() ?? "";
  if (rawUrl.length === 0) {
    return;
  }
  if (feedbackEl !== null) {
    feedbackEl.textContent = "جارٍ إرسال الرابط إلى نافذة تأكيد Vortex...";
  }
  void chrome.runtime
    .sendMessage({ kind: "vortex.quickDownload", url: rawUrl })
    .then((res: unknown) => {
      const r = res as QuickReply;
      if (feedbackEl !== null) {
        feedbackEl.textContent = r.ok
          ? "تم إرسال الرابط إلى نافذة تأكيد Vortex بنجاح"
          : `تعذر الإرسال: ${r.error ?? "تطبيق Vortex غير متصل"}`;
      }
      if (r.ok && urlInputEl !== null) {
        urlInputEl.value = "";
      }
    });
});

optionsBtnEl?.addEventListener("click", () => {
  void chrome.runtime.openOptionsPage();
});
