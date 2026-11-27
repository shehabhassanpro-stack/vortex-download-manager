/**
 * 3-Tier `VTX-*` Error Display & Calm Degraded Mode Banner (`P1-T31` / `Design-System.md` §5).
 *
 * Tier 1: Calm localized user message
 * Tier 2: Actionable remediation buttons
 * Tier 3: Expandable sanitized technical details (`VTX-*` code + `trace_id` with `.vtx-ltr-isolate`)
 */

import { useState } from "react";
import { useTranslation } from "react-i18next";
import type { ApiError, SuggestedAction } from "@vortex/protocol";
import { normalizeApiError } from "../../bindings/client.js";

interface DegradedBannerProps {
  readonly visible: boolean;
  readonly onRetry: () => void;
}

export function DegradedModeBanner({ visible, onRetry }: DegradedBannerProps) {
  const { t } = useTranslation();
  if (!visible) {
    return null;
  }
  return (
    <div
      role="status"
      aria-live="polite"
      style={{
        display: "flex",
        alignItems: "center",
        justifyContent: "space-between",
        gap: "12px",
        paddingBlock: "8px",
        paddingInline: "14px",
        borderRadius: "var(--vtx-radius-card)",
        background: "var(--vtx-bg-elevated)",
        border: "1px solid var(--vtx-status-warning)",
      }}
    >
      <span style={{ color: "var(--vtx-status-warning)", fontWeight: 500 }}>
        {t("app.degradedBanner")}
      </span>
      <button type="button" className="vtx-btn" onClick={onRetry}>
        {t("app.retryNow")}
      </button>
    </div>
  );
}

interface VtxErrorCardProps {
  readonly error: ApiError;
  readonly onAction?: (action: SuggestedAction) => void;
}

export function VtxErrorCard({ error, onAction }: VtxErrorCardProps) {
  const { t } = useTranslation();
  const [expanded, setExpanded] = useState(false);
  const [copied, setCopied] = useState(false);

  const safeError = normalizeApiError(error);
  const actions = Array.isArray(safeError.suggestedActions) ? safeError.suggestedActions : [];

  const friendlyText = t(`errors.${safeError.code}`, {
    ...safeError.messageParams,
    defaultValue: t("errors.fallback"),
  });

  const copyDiagnostics = (): void => {
    const snippet = `[${safeError.code}] trace_id=${safeError.traceId} detail=${safeError.technicalDetail}`;
    void navigator.clipboard?.writeText(snippet);
    setCopied(true);
    setTimeout(() => setCopied(false), 2000);
  };

  return (
    <div
      role="region"
      aria-label={safeError.code}
      style={{
        paddingBlock: "10px",
        paddingInline: "12px",
        borderRadius: "var(--vtx-radius-card)",
        background: "var(--vtx-bg-elevated)",
        border: "1px solid var(--vtx-border-strong)",
        display: "flex",
        flexDirection: "column",
        gap: "8px",
      }}
    >
      {/* Tier 1: Human-friendly localized explanation */}
      <div style={{ fontWeight: 500, color: "var(--vtx-status-warning)" }}>{friendlyText}</div>

      {/* Tier 2: Suggested Remediation Actions */}
      <div style={{ display: "flex", flexWrap: "wrap", gap: "8px", alignItems: "center" }}>
        {actions.map((act) => (
          <button
            key={act}
            type="button"
            className="vtx-btn"
            data-action={act}
            onClick={() => {
              if (act === "CopyDiagnostics") {
                copyDiagnostics();
              } else {
                onAction?.(act);
              }
            }}
          >
            {act === "CopyDiagnostics" && copied
              ? t("errors.copied")
              : t(`errors.actions.${act}`, { defaultValue: act })}
          </button>
        ))}

        <button
          type="button"
          className="vtx-btn"
          aria-expanded={expanded}
          onClick={() => setExpanded((prev) => !prev)}
        >
          {t("errors.technicalToggle")}
        </button>
      </div>

      {/* Tier 3: Expandable Sanitized Technical Details */}
      {expanded && (
        <div
          style={{
            paddingBlock: "8px",
            paddingInline: "10px",
            borderRadius: "var(--vtx-radius-control)",
            background: "var(--vtx-bg-canvas)",
            fontSize: "12px",
            color: "var(--vtx-text-secondary)",
            display: "flex",
            flexDirection: "column",
            gap: "4px",
          }}
        >
          <div>
            Code: <code className="vtx-ltr-isolate">{safeError.code}</code> · Trace:{" "}
            <code className="vtx-ltr-isolate">{safeError.traceId}</code>
          </div>
          {safeError.technicalDetail.length > 0 && (
            <div className="vtx-ltr-isolate">{safeError.technicalDetail}</div>
          )}
        </div>
      )}
    </div>
  );
}
