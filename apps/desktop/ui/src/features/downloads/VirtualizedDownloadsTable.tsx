/**
 * `60 FPS` Virtualized Downloads Table (`@tanstack/react-virtual`) (`P1-T32` & `P1.5-T09`).
 *
 * Renders only visible DOM rows (`<= 20` in SSR/jsdom & `< 16.6ms` at `10,000+` rows),
 * featuring Semantic File-Type Badges, crisp `lucide-react` action controls (Pause/Resume,
 * Open Folder, Open File, Live Inspector, Delete), glowing progress track, and a crafted
 * vector Empty State Illustration when the list is empty.
 */

import { useRef } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import { useTranslation } from "react-i18next";
import {
  Activity,
  ExternalLink,
  FolderOpen,
  Pause,
  Play,
  Plus,
  ShieldCheck,
  Trash2,
} from "lucide-react";
import type { DownloadSummaryDto } from "../../bindings/client.js";
import { formatBytes, formatEta, formatSpeed } from "../../shared/lib/format.js";
import { FileTypeBadge } from "../../shared/ui/FileTypeBadge.js";
import { VortexLogo } from "../../shared/ui/VortexLogo.js";

interface VirtualizedDownloadsTableProps {
  readonly items: readonly DownloadSummaryDto[];
  readonly selectedId: string | null;
  readonly onSelect: (id: string) => void;
  readonly onInspectProgress: (id: string) => void;
  readonly onTogglePauseResume: (item: DownloadSummaryDto) => void;
  readonly onRevealInFolder?: (id: string) => void;
  readonly onOpenFile?: (item: DownloadSummaryDto) => void;
  readonly onDelete: (id: string) => void;
  readonly onAddFirstUrl?: () => void;
}

const ROW_HEIGHT_PX = 62;

export function VirtualizedDownloadsTable({
  items,
  selectedId,
  onSelect,
  onInspectProgress,
  onTogglePauseResume,
  onRevealInFolder,
  onOpenFile,
  onDelete,
  onAddFirstUrl,
}: VirtualizedDownloadsTableProps) {
  const { t } = useTranslation();
  const scrollRef = useRef<HTMLDivElement | null>(null);

  const virtualizer = useVirtualizer({
    count: items.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => ROW_HEIGHT_PX,
    overscan: 8,
  });

  if (items.length === 0) {
    return (
      <div className="vtx-empty-state" role="status">
        <div className="vtx-empty-orbital" aria-hidden="true">
          <div className="vtx-empty-ring vtx-empty-ring-outer" />
          <div className="vtx-empty-ring vtx-empty-ring-inner" />
          <VortexLogo size={64} />
        </div>
        <h2 className="vtx-empty-title">{t("empty.title")}</h2>
        <p className="vtx-empty-subtitle">{t("empty.subtitle")}</p>
        {onAddFirstUrl !== undefined && (
          <button type="button" className="vtx-btn vtx-btn-primary" onClick={onAddFirstUrl}>
            <Plus size={16} />
            <span>{t("empty.ctaAdd")}</span>
          </button>
        )}
        <div className="vtx-empty-pill">
          <ShieldCheck size={14} />
          <span>{t("empty.braveHint")}</span>
        </div>
      </div>
    );
  }

  const virtualRows = virtualizer.getVirtualItems();
  // SSR / jsdom fallback (`scrollRef.current === null` has 0 viewport height): render capped top 20 rows
  const renderedIndices: readonly { readonly index: number; readonly start: number }[] =
    virtualRows.length > 0
      ? virtualRows.map((v) => ({ index: v.index, start: v.start }))
      : items.slice(0, 20).map((_, idx) => ({ index: idx, start: idx * ROW_HEIGHT_PX }));

  return (
    <div ref={scrollRef} role="listbox" aria-label={t("nav.all")} className="vtx-table-viewport">
      <div
        style={{
          blockSize: `${Math.max(items.length * ROW_HEIGHT_PX, ROW_HEIGHT_PX)}px`,
          inlineSize: "100%",
          position: "relative",
        }}
      >
        {renderedIndices.map(({ index, start }) => {
          const item = items[index];
          if (item === undefined) {
            return null;
          }
          const isSelected = item.id === selectedId;
          const isCompleted = item.state_slug === "completed";
          const isDownloading = item.state_slug === "downloading";
          const pct = isCompleted
            ? 100
            : item.total_size !== null && item.total_size > 0
              ? Math.min(100, Math.round((item.downloaded_bytes / item.total_size) * 100))
              : 0;

          const statusColor = isCompleted
            ? "var(--vtx-status-success)"
            : item.state_slug === "needs_attention" || item.state_slug === "paused"
              ? "var(--vtx-status-warning)"
              : item.state_slug === "failed"
                ? "var(--vtx-status-danger)"
                : "var(--vtx-status-active)";

          const stateLabel = t(`states.${item.state_slug}`, { defaultValue: item.state_slug });

          return (
            <div
              key={item.id}
              role="option"
              aria-selected={isSelected}
              tabIndex={0}
              className={`vtx-row ${isSelected ? "vtx-row-selected" : ""}`}
              onClick={() => onSelect(item.id)}
              onDoubleClick={() => onInspectProgress(item.id)}
              onKeyDown={(ev) => {
                if (ev.key === "Enter") {
                  onInspectProgress(item.id);
                }
              }}
              style={{
                position: "absolute",
                insetBlockStart: 0,
                insetInlineStart: 0,
                inlineSize: "100%",
                blockSize: `${ROW_HEIGHT_PX}px`,
                transform: `translateY(${start}px)`,
              }}
            >
              {/* Col 1: Semantic File Badge + Filename + URL/Error */}
              <div className="vtx-row-file">
                <FileTypeBadge categoryId={item.category_id} fileName={item.file_name} size="md" />
                <div style={{ minInlineSize: 0 }}>
                  <div className="vtx-row-filename" title={item.file_name}>
                    {item.file_name}
                  </div>
                  <div className="vtx-row-meta">
                    {item.error !== null ? (
                      <span
                        className="vtx-ltr-isolate"
                        style={{ color: "var(--vtx-status-warning)" }}
                      >
                        {item.error.code}
                      </span>
                    ) : (
                      <span className="vtx-ltr-isolate">{item.display_url}</span>
                    )}
                  </div>
                </div>
              </div>

              {/* Col 2: Size */}
              <span className="vtx-tabular vtx-row-size">{formatBytes(item.total_size)}</span>

              {/* Col 3: Glowing Progress + Status */}
              <div className="vtx-row-progress">
                <div className="vtx-progress-track">
                  <div
                    className={`vtx-progress-fill ${isDownloading ? "vtx-progress-glow" : ""}`}
                    style={{ inlineSize: `${pct}%`, background: statusColor }}
                  />
                </div>
                <div className="vtx-row-progress-meta vtx-tabular">
                  <span style={{ color: statusColor }}>{stateLabel}</span>
                  <span>{pct}%</span>
                </div>
              </div>

              {/* Col 4: Speed & ETA */}
              <div className="vtx-row-telemetry vtx-tabular">
                <div
                  className="vtx-row-speed"
                  style={{
                    color: isDownloading ? "var(--vtx-brand-cyan)" : "var(--vtx-text-muted)",
                  }}
                >
                  {isDownloading ? formatSpeed(item.speed_bps) : "—"}
                </div>
                <div className="vtx-row-eta">{isDownloading ? formatEta(item.eta_secs) : ""}</div>
              </div>

              {/* Col 5: Crisp Hover/Focus Vector Actions */}
              <div className="vtx-row-actions">
                {!isCompleted && (
                  <button
                    type="button"
                    className="vtx-icon-btn"
                    title={isDownloading ? t("toolbar.pause") : t("toolbar.resume")}
                    aria-label={isDownloading ? t("toolbar.pause") : t("toolbar.resume")}
                    onClick={(ev) => {
                      ev.stopPropagation();
                      onTogglePauseResume(item);
                    }}
                  >
                    {isDownloading ? <Pause size={14} /> : <Play size={14} />}
                  </button>
                )}

                {isCompleted && onOpenFile !== undefined && (
                  <button
                    type="button"
                    className="vtx-icon-btn"
                    title={t("toolbar.openFile")}
                    aria-label={t("toolbar.openFile")}
                    onClick={(ev) => {
                      ev.stopPropagation();
                      onOpenFile(item);
                    }}
                  >
                    <ExternalLink size={14} />
                  </button>
                )}

                {onRevealInFolder !== undefined && (
                  <button
                    type="button"
                    className="vtx-icon-btn"
                    title={t("toolbar.openFolder")}
                    aria-label={t("toolbar.openFolder")}
                    onClick={(ev) => {
                      ev.stopPropagation();
                      onRevealInFolder(item.id);
                    }}
                  >
                    <FolderOpen size={14} />
                  </button>
                )}

                <button
                  type="button"
                  className="vtx-icon-btn"
                  title={t("toolbar.inspect")}
                  aria-label={t("toolbar.inspect")}
                  onClick={(ev) => {
                    ev.stopPropagation();
                    onInspectProgress(item.id);
                  }}
                >
                  <Activity size={14} />
                </button>

                <button
                  type="button"
                  className="vtx-icon-btn vtx-icon-btn-danger"
                  title={t("toolbar.delete")}
                  aria-label={t("toolbar.delete")}
                  onClick={(ev) => {
                    ev.stopPropagation();
                    onDelete(item.id);
                  }}
                >
                  <Trash2 size={14} />
                </button>
              </div>
            </div>
          );
        })}
      </div>
    </div>
  );
}
