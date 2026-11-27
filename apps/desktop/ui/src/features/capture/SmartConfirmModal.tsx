/**
 * Smart Capture Confirmation & New Download Modal (`P1.5-T04`, `P1.5-T06`).
 *
 * Dual-purpose flagship confirmation shield:
 * 1. **Browser Capture Mode** (renders the front of `stagedQueue`, FIFO clickjack-safe):
 *    - `350ms` Input Grace Lock preventing accidental clicks on rapid ad popups (`TH-20`)
 *    - Stacked Ad-Burst Quarantine counter + 1-click `[رفض الكل]`
 *    - Magic-Byte Security Shield (`VerifiedMatch` / `Safe` / `Caution` / `Danger` masquerade locking `Enter`!)
 *    - Cross-Origin Redirect Trail (`referrer -> origin`) + 1-click `[حظر هذا الموقع]`
 *    - Native Windows `IFileOpenDialog` `[استعراض...]` + live drive free space check
 *    - Instant `[رفض]` closing the socket & freeing RAM in `0ms`
 * 2. **Manual URL Mode**: Clean URL entry with warm `prepareAddDownload` pre-stream + security check!
 * 3. **Standalone Compact Tray Popup Mode** (`standaloneWindow === true`): Full-bleed inside `?mode=confirm`.
 */

import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  AlertOctagon,
  AlertTriangle,
  Ban,
  ChevronDown,
  ChevronUp,
  Clock,
  Download,
  ExternalLink,
  FolderOpen,
  HardDrive,
  ShieldAlert,
  ShieldCheck,
  Trash2,
  X,
  Zap,
} from "lucide-react";
import {
  vortexApi,
  type AddDownloadPreviewDto,
  type ContentSecurityAssessment,
  type StagedDownloadDto,
} from "../../bindings/client.js";
import { formatBytes, formatBytesDual } from "../../shared/lib/format.js";
import { FileTypeBadge } from "../../shared/ui/FileTypeBadge.js";
import { VortexLogo } from "../../shared/ui/VortexLogo.js";

/** Extracts file extension (supporting compound `.tar.gz`/`.tar.zst`/`.tar.xz`) for `UX-M04` protection. */
function extractExtension(name: string): string {
  const lower = name.toLowerCase();
  for (const compound of [".tar.gz", ".tar.xz", ".tar.zst", ".tar.bz2"]) {
    if (lower.endsWith(compound) && name.length > compound.length) {
      return name.slice(name.length - compound.length);
    }
  }
  const lastDot = name.lastIndexOf(".");
  if (lastDot <= 0 || lastDot === name.length - 1) {
    return "";
  }
  const ext = name.slice(lastDot);
  return ext.length <= 12 && !ext.includes(" ") ? ext : "";
}

interface SmartConfirmModalProps {
  readonly stagedQueue: readonly StagedDownloadDto[];
  readonly manualMode: boolean;
  readonly initialUrl?: string;
  readonly defaultSaveDir?: string;
  readonly standaloneWindow?: boolean;
  readonly onConfirmed: () => void;
  readonly onDiscardStaged: (stagedId: string) => Promise<void>;
  readonly onDiscardAllStaged: () => Promise<void>;
  readonly onBlockHost: (stagedId: string, host: string) => Promise<void>;
  readonly onJumpToExisting?: (existingId: string) => void;
  readonly onCloseManual: () => void;
}

function SecurityShieldBanner({ security }: { readonly security: ContentSecurityAssessment }) {
  const { t } = useTranslation();
  const isBidi = security.warnings.includes("bidi_filename_spoofing");
  const isWeaponized = security.warnings.includes("weaponized_script_or_shortcut");
  const isDiskImage = security.warnings.includes("mountable_disk_image");
  const isDanger = security.risk_level === "danger";
  const isHtmlTrap = security.warnings.includes("html_trap");
  const isExec = security.warnings.includes("high_risk_executable");
  const isVerified = security.risk_level === "verified_match";

  let toneClass = "vtx-shield-safe";
  let title = t("confirm.safeTitle");
  let desc = t("confirm.safeDesc");
  let Icon = ShieldCheck;

  if (isBidi) {
    toneClass = "vtx-shield-danger";
    title = t("confirm.bidiTitle");
    desc = t("confirm.bidiDesc");
    Icon = AlertOctagon;
  } else if (isWeaponized) {
    toneClass = "vtx-shield-danger";
    title = t("confirm.weaponizedTitle", { sig: security.detected_signature ?? "SCRIPT" });
    desc = t("confirm.weaponizedDesc");
    Icon = AlertOctagon;
  } else if (isDanger) {
    toneClass = "vtx-shield-danger";
    title = t("confirm.masqueradeTitle");
    desc = t("confirm.masqueradeDesc", { sig: security.detected_signature ?? "EXE" });
    Icon = AlertOctagon;
  } else if (isHtmlTrap) {
    toneClass = "vtx-shield-caution";
    title = t("confirm.htmlTrapTitle");
    desc = t("confirm.htmlTrapDesc");
    Icon = AlertTriangle;
  } else if (isDiskImage) {
    toneClass = "vtx-shield-caution";
    title = t("confirm.diskImageTitle");
    desc = t("confirm.diskImageDesc");
    Icon = ShieldAlert;
  } else if (isExec) {
    toneClass = "vtx-shield-caution";
    title = t("confirm.executableTitle");
    desc = t("confirm.executableDesc");
    Icon = ShieldAlert;
  } else if (isVerified) {
    toneClass = "vtx-shield-verified";
    title = t("confirm.verifiedTitle", { sig: security.detected_signature ?? "OK" });
    desc = t("confirm.verifiedDesc");
    Icon = ShieldCheck;
  }

  return (
    <div
      role={isDanger || isHtmlTrap ? "alert" : "status"}
      className={`vtx-shield-banner ${toneClass}`}
    >
      <div className="vtx-shield-icon">
        <Icon size={20} strokeWidth={2} />
      </div>
      <div className="vtx-shield-body">
        <div className="vtx-shield-title">{title}</div>
        <div className="vtx-shield-desc">{desc}</div>
        {security.origin_host.length > 0 && (
          <div className="vtx-shield-origin vtx-ltr-isolate">
            {security.referrer_host !== null && security.cross_origin_redirected
              ? t("confirm.originTrail", {
                  referrer: security.referrer_host,
                  origin: security.origin_host,
                })
              : t("confirm.originOnly", { origin: security.origin_host })}
          </div>
        )}
      </div>
    </div>
  );
}

export function SmartConfirmModal({
  stagedQueue,
  manualMode,
  initialUrl = "",
  defaultSaveDir = "",
  standaloneWindow = false,
  onConfirmed,
  onDiscardStaged,
  onDiscardAllStaged,
  onBlockHost,
  onJumpToExisting,
  onCloseManual,
}: SmartConfirmModalProps) {
  const { t } = useTranslation();
  const activeStaged: StagedDownloadDto | null =
    !manualMode && stagedQueue.length > 0 ? (stagedQueue[0] ?? null) : null;

  const [url, setUrl] = useState(initialUrl);
  const [fileName, setFileName] = useState("");
  const [saveDir, setSaveDir] = useState(defaultSaveDir);
  const [expectedHash, setExpectedHash] = useState("");
  const [overwriteExisting, setOverwriteExisting] = useState(false);
  const [showAdvanced, setShowAdvanced] = useState(false);
  const [manualPreview, setManualPreview] = useState<AddDownloadPreviewDto | null>(null);
  const [freeBytes, setFreeBytes] = useState<number | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const [errorMsg, setErrorMsg] = useState("");
  // `350ms` Input Grace Lock on new browser captures preventing accidental ad-popup clicks (`TH-20`)!
  const [graceLocked, setGraceLocked] = useState(activeStaged !== null);

  // Sync fields whenever the front staged capture item changes
  const activeStagedId = activeStaged?.staged_id ?? null;
  useEffect(() => {
    if (activeStaged === null) {
      setGraceLocked(false);
      return;
    }
    setUrl(activeStaged.url);
    setFileName(activeStaged.suggested_file_name);
    setSaveDir(activeStaged.suggested_save_dir);
    setFreeBytes(activeStaged.free_disk_bytes);
    setExpectedHash("");
    setOverwriteExisting(false);
    setErrorMsg("");
    setGraceLocked(true);
    const lockTimer = setTimeout(() => setGraceLocked(false), 350);
    return () => clearTimeout(lockTimer);
  }, [activeStagedId]);

  // Sync initial URL in manual mode
  useEffect(() => {
    if (!manualMode) return;
    setUrl(initialUrl);
    if (defaultSaveDir.length > 0 && saveDir.length === 0) {
      setSaveDir(defaultSaveDir);
    }
  }, [manualMode, initialUrl, defaultSaveDir]);

  // Manual mode: trigger warm `prepareAddDownload` pre-stream (`16 MiB` RAM ring buffer)
  useEffect(() => {
    if (!manualMode) {
      setManualPreview(null);
      return;
    }
    const trimmed = url.trim();
    if (!trimmed.startsWith("http://") && !trimmed.startsWith("https://")) {
      setManualPreview(null);
      return;
    }
    let cancelled = false;
    const timer = setTimeout(() => {
      void vortexApi
        .prepareAddDownload(trimmed)
        .then((p) => {
          if (cancelled) return;
          setManualPreview(p);
          setFileName(p.suggested_file_name);
          setSaveDir(p.suggested_save_dir);
          setFreeBytes(p.free_disk_bytes);
          setErrorMsg("");
        })
        .catch(() => {
          // Keep user-editable fields if remote server requires direct download or auth
        });
    }, 180);
    return () => {
      cancelled = true;
      clearTimeout(timer);
    };
  }, [manualMode, url]);

  const effectiveSecurity: ContentSecurityAssessment | null =
    activeStaged?.security ?? manualPreview?.security ?? null;
  const effectiveTotalSize: number | null =
    activeStaged?.total_size ?? manualPreview?.total_size ?? null;
  const effectiveSupportsRange: boolean =
    activeStaged?.supports_range ?? manualPreview?.supports_range ?? false;
  const effectiveDuplicate = activeStaged?.duplicate ?? manualPreview?.duplicate ?? null;
  const effectiveCategory = activeStaged?.category_id ?? manualPreview?.category_id ?? "others";
  const originalFileName =
    activeStaged?.suggested_file_name ?? manualPreview?.suggested_file_name ?? "";
  const originalExt = extractExtension(originalFileName);
  const extensionAltered =
    originalExt.length > 0 &&
    fileName.trim().length > 0 &&
    !fileName.trim().toLowerCase().endsWith(originalExt.toLowerCase());
  const isInteractiveViewer =
    manualPreview?.url_provider_hint === "interactive_viewer" ||
    (effectiveSecurity?.warnings.includes("html_trap") ?? false);
  const isDanger = effectiveSecurity?.risk_level === "danger";
  const enterLocked = effectiveSecurity?.block_enter_shortcut ?? false;
  const isLowDisk =
    freeBytes !== null && effectiveTotalSize !== null && effectiveTotalSize > freeBytes;

  const blockableHost =
    activeStaged !== null
      ? (activeStaged.security.referrer_host ?? activeStaged.security.origin_host)
      : "";

  const handleBrowseFolder = async (): Promise<void> => {
    try {
      const picked = await vortexApi.pickFolder(saveDir || undefined);
      if (picked !== null && picked.length > 0) {
        setSaveDir(picked);
        const bytes = await vortexApi.getFreeDiskBytes(picked);
        setFreeBytes(bytes);
      }
    } catch {
      // User cancelled or picker unavailable
    }
  };

  const handleSubmit = async (autoStart: boolean): Promise<void> => {
    if (graceLocked || submitting) return;
    setSubmitting(true);
    setErrorMsg("");
    try {
      const stagedId = activeStaged?.staged_id ?? manualPreview?.staged_id;
      const trimmedFileName = fileName.trim();
      const trimmedSaveDir = saveDir.trim();
      const trimmedHash = expectedHash.trim();
      await vortexApi.addDownload({
        ...(stagedId !== undefined ? { stagedId } : {}),
        url: url.trim(),
        ...(trimmedFileName.length > 0 ? { fileName: trimmedFileName } : {}),
        ...(trimmedSaveDir.length > 0 ? { saveDir: trimmedSaveDir } : {}),
        categoryId: effectiveCategory,
        ...(trimmedHash.length > 0 ? { expectedChecksum: trimmedHash } : {}),
        autoStart,
        overwriteExisting,
      });
      onConfirmed();
    } catch (err) {
      setErrorMsg(err instanceof Error ? err.message : String(err));
    } finally {
      setSubmitting(false);
    }
  };

  const handleDiscard = async (): Promise<void> => {
    if (submitting) return;
    if (activeStaged !== null) {
      await onDiscardStaged(activeStaged.staged_id);
      return;
    }
    if (manualPreview !== null) {
      await vortexApi.discardStagedDownload(manualPreview.staged_id).catch(() => undefined);
    }
    onCloseManual();
  };

  // Keyboard shortcuts (`Esc` to discard/close cleanly!)
  useEffect(() => {
    const onKey = (ev: KeyboardEvent): void => {
      if (ev.key === "Escape") {
        ev.preventDefault();
        void handleDiscard();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  });

  const dialogCard = (
    <div
      role="dialog"
      aria-modal={!standaloneWindow}
      aria-label={activeStaged !== null ? t("confirm.browserTitle") : t("confirm.manualTitle")}
      className={`vtx-confirm-card ${standaloneWindow ? "vtx-confirm-standalone" : ""}`}
      data-testid="smart-confirm-dialog"
    >
      {/* Header */}
      <header className="vtx-confirm-header">
        <div className="vtx-confirm-title-group">
          {standaloneWindow ? (
            <VortexLogo size={32} />
          ) : (
            <FileTypeBadge
              categoryId={effectiveCategory}
              fileName={fileName || "file.bin"}
              size="md"
            />
          )}
          <div style={{ minInlineSize: 0 }}>
            <h2 className="vtx-confirm-heading">
              {activeStaged !== null ? t("confirm.browserTitle") : t("confirm.manualTitle")}
            </h2>
            <div className="vtx-confirm-submeta">
              {activeStaged !== null && (
                <span className="vtx-pill vtx-pill-cyan">
                  <Zap size={12} /> {t("confirm.warmStreamBadge")}
                </span>
              )}
              {effectiveTotalSize !== null && (
                <span className="vtx-pill vtx-tabular">{formatBytesDual(effectiveTotalSize)}</span>
              )}
              {manualPreview?.canonical_url_transformed === true && (
                <span className="vtx-pill vtx-pill-cyan">{t("confirm.canonicalRewritten")}</span>
              )}
              {effectiveSupportsRange && (
                <span className="vtx-pill vtx-pill-success">{t("confirm.rangeSupported")}</span>
              )}
            </div>
          </div>
        </div>

        <div className="vtx-confirm-header-actions">
          {activeStaged !== null && stagedQueue.length > 1 && (
            <span className="vtx-pill vtx-pill-warning vtx-tabular">
              {t("confirm.queueCounter", { current: 1, total: stagedQueue.length })}
            </span>
          )}
          {standaloneWindow && (
            <button
              type="button"
              className="vtx-icon-btn"
              title={t("confirm.openMainApp")}
              aria-label={t("confirm.openMainApp")}
              onClick={() => void vortexApi.openMainWindow()}
            >
              <ExternalLink size={15} />
            </button>
          )}
          <button
            type="button"
            className="vtx-icon-btn"
            aria-label={t("confirm.cancel")}
            onClick={() => void handleDiscard()}
          >
            <X size={16} />
          </button>
        </div>
      </header>

      {/* Ad-Burst Storm Quarantine Banner (`TH-20`) */}
      {activeStaged !== null && (activeStaged.burst_quarantined || stagedQueue.length > 1) && (
        <div role="alert" className="vtx-burst-banner">
          <div className="vtx-burst-text">
            <AlertTriangle size={16} />
            <span>{t("confirm.burstBanner")}</span>
          </div>
          {stagedQueue.length > 1 && (
            <button
              type="button"
              className="vtx-btn vtx-btn-danger-ghost"
              onClick={() => void onDiscardAllStaged()}
            >
              <Trash2 size={14} />
              <span>{t("confirm.discardAll", { count: stagedQueue.length })}</span>
            </button>
          )}
        </div>
      )}

      {/* Body Form */}
      <form
        className="vtx-confirm-body"
        onSubmit={(ev) => {
          ev.preventDefault();
          if (enterLocked) return;
          void handleSubmit(true);
        }}
      >
        {/* Security Shield */}
        {effectiveSecurity !== null && <SecurityShieldBanner security={effectiveSecurity} />}

        {/* Interactive Webpage / HTML Trap Escape Hatch (`UX-M01`) */}
        {isInteractiveViewer && url.trim().startsWith("http") && (
          <div style={{ display: "flex", justifyContent: "flex-end" }}>
            <button
              type="button"
              className="vtx-btn"
              onClick={() => void vortexApi.openExternalUrl(url.trim())}
            >
              <ExternalLink size={14} />
              <span>{t("confirm.openInBrowser")}</span>
            </button>
          </div>
        )}

        {/* URL Field (editable in manual mode, read-only redacted display in browser mode) */}
        {manualMode ? (
          <label className="vtx-field">
            <span className="vtx-field-label">{t("addDialog.urlLabel")}</span>
            <input
              type="url"
              className="vtx-input vtx-ltr-isolate"
              placeholder={t("addDialog.urlPlaceholder")}
              value={url}
              onChange={(e) => setUrl(e.target.value)}
              required
              autoFocus
            />
          </label>
        ) : (
          <div className="vtx-url-preview vtx-ltr-isolate" title={activeStaged?.display_url}>
            {activeStaged?.display_url}
          </div>
        )}

        {/* Duplicate Alert (`UX-M08`) */}
        {effectiveDuplicate !== null && (
          <div role="alert" className="vtx-inline-warn">
            <div className="vtx-inline-warn-head">
              <AlertTriangle size={15} />
              <span>{t("addDialog.duplicateWarn", { name: effectiveDuplicate.file_name })}</span>
            </div>
            <div style={{ display: "flex", flexWrap: "wrap", gap: "10px", alignItems: "center" }}>
              <label className="vtx-checkbox-label">
                <input
                  type="checkbox"
                  checked={overwriteExisting}
                  onChange={(e) => setOverwriteExisting(e.target.checked)}
                />
                <span>{t("addDialog.overwriteToggle")}</span>
              </label>
              {onJumpToExisting !== undefined && (
                <button
                  type="button"
                  className="vtx-btn"
                  onClick={() => {
                    onJumpToExisting(effectiveDuplicate.existing_id);
                    void handleDiscard();
                  }}
                >
                  <ExternalLink size={13} />
                  <span>{t("confirm.jumpToExisting")}</span>
                </button>
              )}
            </div>
          </div>
        )}

        {/* Filename (`UX-M04` stem-only focus selection & extension-deletion guard) */}
        <label className="vtx-field">
          <span className="vtx-field-label">{t("addDialog.nameLabel")}</span>
          <input
            type="text"
            className="vtx-input vtx-ltr-isolate"
            value={fileName}
            onFocus={(e) => {
              const ext = extractExtension(e.currentTarget.value);
              const stemEnd =
                ext.length > 0
                  ? e.currentTarget.value.length - ext.length
                  : e.currentTarget.value.length;
              if (stemEnd > 0) {
                e.currentTarget.setSelectionRange(0, stemEnd);
              }
            }}
            onChange={(e) => setFileName(e.target.value)}
            required
          />
          {extensionAltered && (
            <div role="status" className="vtx-inline-warn" style={{ marginBlockStart: "6px" }}>
              <span>{t("confirm.extRemovedWarn", { ext: originalExt })}</span>
              <button
                type="button"
                className="vtx-btn"
                onClick={() => setFileName((prev) => `${prev.trim()}${originalExt}`)}
              >
                {t("confirm.restoreExt", { ext: originalExt })}
              </button>
            </div>
          )}
        </label>

        {/* Save Directory + Native Win32 Browse Button */}
        <div className="vtx-field">
          <label htmlFor="vtx-confirm-dir" className="vtx-field-label">
            {t("addDialog.dirLabel")}
          </label>
          <div className="vtx-input-group">
            <input
              id="vtx-confirm-dir"
              type="text"
              className="vtx-input vtx-ltr-isolate"
              value={saveDir}
              onChange={(e) => setSaveDir(e.target.value)}
              required
            />
            <button type="button" className="vtx-btn" onClick={() => void handleBrowseFolder()}>
              <FolderOpen size={15} />
              <span>{t("confirm.browse")}</span>
            </button>
          </div>
          {freeBytes !== null && (
            <div className={`vtx-disk-hint vtx-tabular ${isLowDisk ? "vtx-disk-hint-danger" : ""}`}>
              <HardDrive size={13} />
              <span>
                {isLowDisk
                  ? t("confirm.lowDiskWarn")
                  : t("confirm.freeSpace", { free: formatBytes(freeBytes) })}
              </span>
            </div>
          )}
        </div>

        {/* Collapsible Advanced Options */}
        <div>
          <button
            type="button"
            className="vtx-disclosure-btn"
            onClick={() => setShowAdvanced((v) => !v)}
            aria-expanded={showAdvanced}
          >
            {showAdvanced ? <ChevronUp size={14} /> : <ChevronDown size={14} />}
            <span>{t("confirm.advancedToggle")}</span>
          </button>

          {showAdvanced && (
            <div className="vtx-advanced-box">
              <label className="vtx-field">
                <span className="vtx-field-label">{t("addDialog.hashLabel")}</span>
                <input
                  type="text"
                  className="vtx-input vtx-ltr-isolate"
                  placeholder="sha256:... / blake3:..."
                  value={expectedHash}
                  onChange={(e) => setExpectedHash(e.target.value)}
                />
              </label>
              <label className="vtx-checkbox-label">
                <input
                  type="checkbox"
                  checked={overwriteExisting}
                  onChange={(e) => setOverwriteExisting(e.target.checked)}
                />
                <span>{t("addDialog.overwriteToggle")}</span>
              </label>
            </div>
          )}
        </div>

        {errorMsg.length > 0 && (
          <div role="alert" className="vtx-inline-error">
            {errorMsg}
          </div>
        )}

        {/* Action Footer */}
        <footer className="vtx-confirm-footer">
          <div className="vtx-confirm-footer-start">
            <button
              type="button"
              className="vtx-btn vtx-btn-danger-ghost"
              data-testid="btn-discard-staged"
              onClick={() => void handleDiscard()}
            >
              <Trash2 size={14} />
              <span>{activeStaged !== null ? t("confirm.discard") : t("confirm.cancel")}</span>
            </button>

            {activeStaged !== null && blockableHost.length > 0 && (
              <button
                type="button"
                className="vtx-btn vtx-btn-ghost"
                onClick={() => void onBlockHost(activeStaged.staged_id, blockableHost)}
              >
                <Ban size={14} />
                <span>{t("confirm.blockHost", { host: blockableHost })}</span>
              </button>
            )}
          </div>

          <div className="vtx-confirm-footer-end">
            <button
              type="button"
              className="vtx-btn"
              disabled={graceLocked || submitting}
              onClick={() => void handleSubmit(false)}
            >
              <Clock size={14} />
              <span>{t("confirm.addPaused")}</span>
            </button>

            {isDanger ? (
              <button
                type="button"
                className="vtx-btn vtx-btn-danger"
                disabled={graceLocked || submitting}
                data-testid="btn-confirm-start"
                onClick={() => void handleSubmit(true)}
              >
                <AlertOctagon size={14} />
                <span>{t("confirm.startDangerAnyway")}</span>
              </button>
            ) : (
              <button
                type="submit"
                className="vtx-btn vtx-btn-primary"
                disabled={graceLocked || submitting}
                data-testid="btn-confirm-start"
              >
                <Download size={14} />
                <span>{graceLocked ? t("confirm.graceLockHint") : t("confirm.startNow")}</span>
              </button>
            )}
          </div>
        </footer>
      </form>
    </div>
  );

  if (standaloneWindow) {
    return <main className="vtx-standalone-confirm-shell">{dialogCard}</main>;
  }

  return <div className="vtx-modal-scrim">{dialogCard}</div>;
}
