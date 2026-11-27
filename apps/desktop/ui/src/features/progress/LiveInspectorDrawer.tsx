/**
 * Dockable Live Inspector Drawer (`512-Bin` Segment Map + `60s` Speed Chart — `P1.5-T09`).
 *
 * Replaces the old standalone Progress screen and removes ALL hardcoded demo fallbacks so
 * real selected-download telemetry is always shown cleanly alongside the main list.
 */

import { useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { ExternalLink, FolderOpen, Pause, Play, X } from "lucide-react";
import type { SuggestedAction } from "@vortex/protocol";
import {
  vortexApi,
  type DownloadDetailsDto,
  type DownloadSummaryDto,
} from "../../bindings/client.js";
import { SegmentBarCanvas } from "../../SegmentBarCanvas.js";
import { formatBytes, formatBytesDual, formatEta, formatSpeed } from "../../shared/lib/format.js";
import { FileTypeBadge } from "../../shared/ui/FileTypeBadge.js";
import { SpeedChartCanvas } from "../../shared/ui/SpeedChartCanvas.js";
import { VtxErrorCard } from "../../shared/ui/VtxErrorCard.js";

interface LiveInspectorDrawerProps {
  readonly item: DownloadSummaryDto | null;
  readonly liveBins?: readonly number[];
  readonly speedHistory?: readonly number[];
  readonly protocol?: string;
  readonly concurrencyMode?: string;
  readonly theme: "dark" | "light";
  readonly onTogglePauseResume: (item: DownloadSummaryDto) => void;
  readonly onRevealInFolder: (id: string) => void;
  readonly onOpenFile: (item: DownloadSummaryDto) => void;
  readonly onClose: () => void;
}

const EMPTY_512_BINS = new Uint8Array(512);
const FULL_512_BINS = new Uint8Array(512).fill(1);

export function LiveInspectorDrawer({
  item,
  liveBins,
  speedHistory,
  protocol,
  concurrencyMode,
  theme,
  onTogglePauseResume,
  onRevealInFolder,
  onOpenFile,
  onClose,
}: LiveInspectorDrawerProps) {
  const { t } = useTranslation();
  const [details, setDetails] = useState<DownloadDetailsDto | null>(null);

  const itemId = item?.id ?? null;
  useEffect(() => {
    if (itemId === null) {
      setDetails(null);
      return;
    }
    let cancelled = false;
    void vortexApi
      .getDownloadDetails(itemId)
      .then((d) => {
        if (!cancelled) setDetails(d);
      })
      .catch(() => undefined);
    return () => {
      cancelled = true;
    };
  }, [itemId, item?.state_slug]);

  const binsArray = useMemo(() => {
    if (liveBins !== undefined && liveBins.length === 512) {
      return Uint8Array.from(liveBins);
    }
    if (details !== null && details.bins.length === 512) {
      return Uint8Array.from(details.bins);
    }
    if (item?.state_slug === "completed") {
      return FULL_512_BINS;
    }
    return EMPTY_512_BINS;
  }, [details, item?.state_slug, liveBins]);

  const chartSamples = useMemo(() => {
    if (speedHistory !== undefined && speedHistory.length >= 2) {
      return speedHistory;
    }
    return [0, item?.speed_bps ?? 0];
  }, [item?.speed_bps, speedHistory]);

  if (item === null) {
    return null;
  }

  const isCompleted = item.state_slug === "completed";
  const isDownloading = item.state_slug === "downloading";

  return (
    <section className="vtx-inspector-drawer" aria-label={t("progress.title")}>
      <header className="vtx-inspector-head">
        <div className="vtx-inspector-title-group">
          <FileTypeBadge categoryId={item.category_id} fileName={item.file_name} size="sm" />
          <div style={{ minInlineSize: 0 }}>
            <strong className="vtx-inspector-filename">{item.file_name}</strong>
            <div className="vtx-inspector-sub vtx-tabular">
              {formatBytes(item.downloaded_bytes)} / {formatBytesDual(item.total_size)}
            </div>
          </div>
        </div>

        <div className="vtx-inspector-actions">
          {!isCompleted ? (
            <button type="button" className="vtx-btn" onClick={() => onTogglePauseResume(item)}>
              {isDownloading ? <Pause size={14} /> : <Play size={14} />}
              <span>{isDownloading ? t("toolbar.pause") : t("toolbar.resume")}</span>
            </button>
          ) : (
            <button
              type="button"
              className="vtx-btn vtx-btn-primary"
              onClick={() => onOpenFile(item)}
            >
              <ExternalLink size={14} />
              <span>{t("toolbar.openFile")}</span>
            </button>
          )}

          <button type="button" className="vtx-btn" onClick={() => onRevealInFolder(item.id)}>
            <FolderOpen size={14} />
            <span>{t("toolbar.openFolder")}</span>
          </button>

          <button
            type="button"
            className="vtx-icon-btn"
            aria-label={t("progress.closeInspector")}
            onClick={onClose}
          >
            <X size={16} />
          </button>
        </div>
      </header>

      <div className="vtx-inspector-grid">
        <div className="vtx-inspector-visuals">
          <SegmentBarCanvas
            bins={binsArray}
            theme={theme}
            ariaLabel={t("progress.segmentBarLabel")}
          />
          <SpeedChartCanvas
            samplesBps={chartSamples}
            theme={theme}
            ariaLabel={t("progress.speedChartLabel")}
          />
        </div>

        <div className="vtx-inspector-metrics">
          <div className="vtx-metric-tile">
            <span className="vtx-metric-label">{t("progress.speed")}</span>
            <strong className="vtx-tabular" style={{ color: "var(--vtx-brand-cyan)" }}>
              {formatSpeed(item.speed_bps)}
            </strong>
          </div>
          <div className="vtx-metric-tile">
            <span className="vtx-metric-label">{t("progress.eta")}</span>
            <strong className="vtx-tabular">{formatEta(item.eta_secs)}</strong>
          </div>
          <div className="vtx-metric-tile">
            <span className="vtx-metric-label">{t("progress.workers")}</span>
            <strong className="vtx-tabular">
              {item.active_workers}
              {concurrencyMode !== undefined && (
                <span
                  style={{
                    fontSize: "11px",
                    fontWeight: 500,
                    marginInlineStart: "6px",
                    color:
                      concurrencyMode === "capped"
                        ? "var(--vtx-status-warning)"
                        : "var(--vtx-brand-cyan)",
                  }}
                >
                  (
                  {concurrencyMode === "capped"
                    ? t("progress.modeCapped")
                    : concurrencyMode === "auto"
                      ? t("progress.modeAuto")
                      : t("progress.modeFixed")}
                  )
                </span>
              )}
            </strong>
          </div>
          <div className="vtx-metric-tile">
            <span className="vtx-metric-label">
              {protocol !== undefined ? t("progress.protocol") : t("progress.resumeCap")}
            </span>
            <strong
              className="vtx-ltr-isolate"
              style={{
                color:
                  protocol === "HTTP/2" ? "var(--vtx-brand-cyan)" : "var(--vtx-status-success)",
              }}
            >
              {protocol !== undefined
                ? protocol
                : item.supports_range
                  ? t("progress.supported")
                  : t("progress.singleStream")}
            </strong>
          </div>
        </div>
      </div>

      {item.error !== null && (
        <VtxErrorCard
          error={item.error}
          onAction={(action: SuggestedAction) => {
            if (action === "OpenReferrerInBrowser" || action === "RefreshLink") {
              void vortexApi.openDownloadReferrer(item.id);
            } else if (action === "SyncSystemClock") {
              void vortexApi.openExternalUrl("ms-settings:dateandtime");
            } else if (action === "OpenWindowsSecurity") {
              void vortexApi.openExternalUrl("windowsdefender://threat");
            } else if (action === "Retry" || action === "RestartFromBeginning") {
              onTogglePauseResume(item);
            } else if (
              action === "ChangeFolder" ||
              action === "FreeDiskSpace" ||
              action === "ReconnectDrive"
            ) {
              onRevealInFolder(item.id);
            }
          }}
        />
      )}
    </section>
  );
}
