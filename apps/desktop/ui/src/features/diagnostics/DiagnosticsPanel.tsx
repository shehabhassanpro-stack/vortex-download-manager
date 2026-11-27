/**
 * Diagnostics & Live Logs Modal (`P1-T34` & `P1.5-T10`).
 *
 * Crisp `lucide-react` health status cards, Native Host HKCU repair, 30-minute DEBUG toggle,
 * color-coded redacted FlightRecorder logs, and Zero-Leakage `.zip` Diagnostic Bundle preview.
 */

import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  AlertTriangle,
  Cable,
  CheckCircle2,
  Database,
  Globe,
  HardDrive,
  PackageCheck,
  ShieldCheck,
  Wrench,
  X,
} from "lucide-react";
import {
  vortexApi,
  type DiagnosticBundleReportDto,
  type HealthStatusDto,
  type LogEntryDto,
} from "../../bindings/client.js";
import { formatBytes } from "../../shared/lib/format.js";

interface DiagnosticsPanelProps {
  readonly onClose?: () => void;
}

export function DiagnosticsPanel({ onClose }: DiagnosticsPanelProps) {
  const { t } = useTranslation();
  const [health, setHealth] = useState<HealthStatusDto | null>(null);
  const [logs, setLogs] = useState<readonly LogEntryDto[]>([]);
  const [bundlePreview, setBundlePreview] = useState<DiagnosticBundleReportDto | null>(null);
  const [exportStatus, setExportStatus] = useState("");

  const reloadDiagnostics = async (): Promise<void> => {
    const [h, recent] = await Promise.all([
      vortexApi.getHealthStatus(),
      vortexApi.getRecentLogs(200),
    ]);
    setHealth(h);
    setLogs(recent);
  };

  useEffect(() => {
    void reloadDiagnostics();
  }, []);

  const panelBody = (
    <section
      role={onClose !== undefined ? "dialog" : "region"}
      {...(onClose !== undefined ? { "aria-modal": true } : {})}
      aria-label={t("diagnostics.title")}
      className="vtx-diagnostics-card"
    >
      <header className="vtx-confirm-header">
        <div className="vtx-confirm-title-group">
          <ShieldCheck size={18} />
          <h2 className="vtx-confirm-heading">{t("diagnostics.title")}</h2>
        </div>
        {onClose !== undefined && (
          <button
            type="button"
            className="vtx-icon-btn"
            aria-label={t("diagnostics.close")}
            onClick={onClose}
          >
            <X size={16} />
          </button>
        )}
      </header>

      <div className="vtx-diagnostics-body">
        {health !== null && (
          <div className="vtx-health-grid">
            <div className="vtx-health-card">
              <div className="vtx-health-label">
                <Database size={14} /> Core & SQLite
              </div>
              <strong
                className="vtx-health-value"
                style={{
                  color: health.db_ok ? "var(--vtx-status-success)" : "var(--vtx-status-danger)",
                }}
              >
                {health.core_ok && health.db_ok ? (
                  <>
                    <CheckCircle2 size={15} /> Healthy (WAL)
                  </>
                ) : (
                  <>
                    <AlertTriangle size={15} /> Degraded
                  </>
                )}
              </strong>
            </div>

            <div className="vtx-health-card">
              <div className="vtx-health-label">
                <Cable size={14} /> Named Pipe IPC
              </div>
              <strong
                className="vtx-health-value"
                style={{
                  color: health.pipe_ok ? "var(--vtx-status-success)" : "var(--vtx-status-warning)",
                }}
              >
                {health.pipe_ok ? (
                  <>
                    <CheckCircle2 size={15} /> Active (Owner SID)
                  </>
                ) : (
                  <>
                    <AlertTriangle size={15} /> Offline
                  </>
                )}
              </strong>
            </div>

            <div className="vtx-health-card">
              <div className="vtx-health-label">
                <HardDrive size={14} /> Free Disk
              </div>
              <strong className="vtx-health-value vtx-tabular">
                {formatBytes(health.free_disk_bytes)}
              </strong>
            </div>

            <div className="vtx-health-card">
              <div className="vtx-health-label">
                <Globe size={14} /> Browsers (HKCU)
              </div>
              <div className="vtx-browser-badges">
                {health.browsers.map((b) => (
                  <span
                    key={b.browser}
                    className={`vtx-pill ${b.registered ? "vtx-pill-success" : "vtx-pill-warning"}`}
                  >
                    {b.browser}
                  </span>
                ))}
              </div>
            </div>
          </div>
        )}

        <div style={{ display: "flex", flexWrap: "wrap", gap: "10px", alignItems: "center" }}>
          <button
            type="button"
            className="vtx-btn"
            onClick={() => {
              void vortexApi.repairBrowserBridge().then(async () => {
                await reloadDiagnostics();
                setExportStatus(t("diagnostics.repairDone"));
              });
            }}
          >
            <Wrench size={14} />
            <span>{t("diagnostics.repairBridge")}</span>
          </button>

          <button
            type="button"
            className="vtx-btn vtx-btn-primary"
            onClick={() => {
              void vortexApi.previewDiagnosticBundle().then((rep) => {
                setBundlePreview(rep);
              });
            }}
          >
            <PackageCheck size={14} />
            <span>{t("diagnostics.exportBundle")}</span>
          </button>
        </div>

        <label className="vtx-checkbox-label">
          <input
            type="checkbox"
            checked={health?.debug_mode_active ?? false}
            onChange={(ev) => {
              void vortexApi.setDebugMode(ev.target.checked).then(() => reloadDiagnostics());
            }}
          />
          <span>{t("diagnostics.debugToggle")}</span>
        </label>

        {exportStatus.length > 0 && (
          <div role="status" className="vtx-status-inline">
            <CheckCircle2 size={14} />
            <span>{exportStatus}</span>
          </div>
        )}

        {bundlePreview !== null && (
          <div
            role="dialog"
            aria-label={t("diagnostics.previewTitle")}
            className="vtx-bundle-preview"
          >
            <strong>
              {t("diagnostics.previewTitle")} (
              <span className="vtx-tabular">{formatBytes(bundlePreview.total_zip_bytes)}</span>)
            </strong>
            <div className="vtx-status-inline">
              <ShieldCheck size={14} />
              <span>{t("diagnostics.zeroLeakNote")}</span>
            </div>
            {bundlePreview.files.map((f) => (
              <div key={f.name} className="vtx-bundle-file">
                <div>
                  <code className="vtx-ltr-isolate">{f.name}</code> ·{" "}
                  <span className="vtx-tabular">{formatBytes(f.size_bytes)}</span>
                </div>
                <pre className="vtx-ltr-isolate vtx-bundle-excerpt">{f.preview_excerpt}</pre>
              </div>
            ))}
            <div className="vtx-confirm-footer-end">
              <button type="button" className="vtx-btn" onClick={() => setBundlePreview(null)}>
                {t("diagnostics.closePreview")}
              </button>
              <button
                type="button"
                className="vtx-btn vtx-btn-primary"
                onClick={() => {
                  void vortexApi.exportDiagnosticBundle().then((saved) => {
                    setExportStatus(saved.bundle_path);
                    setBundlePreview(null);
                  });
                }}
              >
                {t("diagnostics.confirmExport")}
              </button>
            </div>
          </div>
        )}

        <div role="log" aria-label="Flight Recorder Live Logs" className="vtx-log-viewer">
          {logs.map((entry, idx) => (
            <div key={`${entry.timestamp}-${idx}`} className="vtx-log-line vtx-ltr-isolate">
              <span className={`vtx-log-level vtx-log-level-${entry.level.toLowerCase()}`}>
                {entry.level}
              </span>
              <span className="vtx-log-target">{entry.target}</span>
              <span className="vtx-log-msg">{entry.message}</span>
            </div>
          ))}
        </div>
      </div>
    </section>
  );

  if (onClose !== undefined) {
    return <div className="vtx-modal-scrim">{panelBody}</div>;
  }
  return panelBody;
}
