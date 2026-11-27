/**
 * Native Settings Modal (`P1.5-T10`).
 *
 * Lets the user manage default download folder (with native Windows `IFileOpenDialog`),
 * max connections (`1..32`), max concurrent downloads (`1..16`), browser confirmation policy
 * (`اسألني دائماً` default), excluded hosts tags, theme, and language.
 */

import { useState } from "react";
import { useTranslation } from "react-i18next";
import { Check, FolderOpen, Gauge, Plus, Shield, SlidersHorizontal, X } from "lucide-react";
import { vortexApi, type AppConfigDto } from "../../bindings/client.js";

interface SettingsModalProps {
  readonly config: AppConfigDto;
  readonly theme: "dark" | "light";
  readonly onChangeTheme: (theme: "dark" | "light") => void;
  readonly onChangeLanguage: (lang: "ar" | "en") => void;
  readonly onSave: (next: AppConfigDto) => Promise<void>;
  readonly onClose: () => void;
}

type SettingsTab = "general" | "performance" | "capture";

export function SettingsModal({
  config,
  theme,
  onChangeTheme,
  onChangeLanguage,
  onSave,
  onClose,
}: SettingsModalProps) {
  const { t, i18n } = useTranslation();
  const [tab, setTab] = useState<SettingsTab>("general");
  const [draft, setDraft] = useState<AppConfigDto>(config);
  const [hostInput, setHostInput] = useState("");
  const [savedNotice, setSavedNotice] = useState(false);

  const handlePickDefaultDir = async (): Promise<void> => {
    const picked = await vortexApi.pickFolder(draft.default_download_dir);
    if (picked !== null && picked.length > 0) {
      setDraft((prev) => ({ ...prev, default_download_dir: picked }));
    }
  };

  const handleAddHost = (): void => {
    const clean = hostInput.trim().toLowerCase();
    if (clean.length === 0) return;
    if (!draft.capture_rules.excluded_hosts.includes(clean)) {
      setDraft((prev) => ({
        ...prev,
        capture_rules: {
          ...prev.capture_rules,
          excluded_hosts: [...prev.capture_rules.excluded_hosts, clean],
        },
      }));
    }
    setHostInput("");
  };

  const handleRemoveHost = (host: string): void => {
    setDraft((prev) => ({
      ...prev,
      capture_rules: {
        ...prev.capture_rules,
        excluded_hosts: prev.capture_rules.excluded_hosts.filter((h) => h !== host),
      },
    }));
  };

  const handleSubmit = async (): Promise<void> => {
    await onSave(draft);
    setSavedNotice(true);
    setTimeout(() => setSavedNotice(false), 2500);
  };

  return (
    <div className="vtx-modal-scrim">
      <div
        role="dialog"
        aria-modal="true"
        aria-label={t("settings.title")}
        className="vtx-settings-card"
      >
        <header className="vtx-confirm-header">
          <div className="vtx-confirm-title-group">
            <SlidersHorizontal size={18} />
            <h2 className="vtx-confirm-heading">{t("settings.title")}</h2>
          </div>
          <button
            type="button"
            className="vtx-icon-btn"
            aria-label={t("settings.close")}
            onClick={onClose}
          >
            <X size={16} />
          </button>
        </header>

        <div className="vtx-settings-layout">
          <nav className="vtx-settings-rail" aria-label={t("settings.title")}>
            <button
              type="button"
              className={`vtx-nav-item ${tab === "general" ? "vtx-nav-item-active" : ""}`}
              onClick={() => setTab("general")}
            >
              <span className="vtx-nav-label">
                <FolderOpen size={15} />
                {t("settings.general")}
              </span>
            </button>
            <button
              type="button"
              className={`vtx-nav-item ${tab === "performance" ? "vtx-nav-item-active" : ""}`}
              onClick={() => setTab("performance")}
            >
              <span className="vtx-nav-label">
                <Gauge size={15} />
                {t("settings.performance")}
              </span>
            </button>
            <button
              type="button"
              className={`vtx-nav-item ${tab === "capture" ? "vtx-nav-item-active" : ""}`}
              onClick={() => setTab("capture")}
            >
              <span className="vtx-nav-label">
                <Shield size={15} />
                {t("settings.capture")}
              </span>
            </button>
          </nav>

          <div className="vtx-settings-content">
            {tab === "general" && (
              <>
                <div className="vtx-field">
                  <label htmlFor="vtx-settings-dir" className="vtx-field-label">
                    {t("settings.defaultDir")}
                  </label>
                  <div className="vtx-input-group">
                    <input
                      id="vtx-settings-dir"
                      type="text"
                      className="vtx-input vtx-ltr-isolate"
                      value={draft.default_download_dir}
                      onChange={(e) =>
                        setDraft((prev) => ({ ...prev, default_download_dir: e.target.value }))
                      }
                    />
                    <button
                      type="button"
                      className="vtx-btn"
                      onClick={() => void handlePickDefaultDir()}
                    >
                      <FolderOpen size={15} />
                      <span>{t("confirm.browse")}</span>
                    </button>
                  </div>
                </div>

                <div className="vtx-field">
                  <span className="vtx-field-label">{t("settings.themeLabel")}</span>
                  <div className="vtx-segmented">
                    <button
                      type="button"
                      className={`vtx-btn ${theme === "dark" ? "vtx-btn-tab-active" : ""}`}
                      onClick={() => {
                        onChangeTheme("dark");
                        setDraft((prev) => ({ ...prev, theme: "dark" }));
                      }}
                    >
                      {t("settings.themeDark")}
                    </button>
                    <button
                      type="button"
                      className={`vtx-btn ${theme === "light" ? "vtx-btn-tab-active" : ""}`}
                      onClick={() => {
                        onChangeTheme("light");
                        setDraft((prev) => ({ ...prev, theme: "light" }));
                      }}
                    >
                      {t("settings.themeLight")}
                    </button>
                  </div>
                </div>

                <div className="vtx-field">
                  <span className="vtx-field-label">{t("settings.langLabel")}</span>
                  <div className="vtx-segmented">
                    <button
                      type="button"
                      className={`vtx-btn ${i18n.language === "ar" ? "vtx-btn-tab-active" : ""}`}
                      onClick={() => {
                        onChangeLanguage("ar");
                        setDraft((prev) => ({ ...prev, language: "ar" }));
                      }}
                    >
                      العربية (RTL)
                    </button>
                    <button
                      type="button"
                      className={`vtx-btn ${i18n.language === "en" ? "vtx-btn-tab-active" : ""}`}
                      onClick={() => {
                        onChangeLanguage("en");
                        setDraft((prev) => ({ ...prev, language: "en" }));
                      }}
                    >
                      English (LTR)
                    </button>
                  </div>
                </div>
              </>
            )}

            {tab === "performance" && (
              <>
                <label className="vtx-field">
                  <span className="vtx-field-label">
                    {t("settings.maxConnections")}:{" "}
                    <strong className="vtx-tabular">{draft.max_connections_per_download}</strong>
                  </span>
                  <input
                    type="range"
                    min={1}
                    max={32}
                    value={draft.max_connections_per_download}
                    onChange={(e) =>
                      setDraft((prev) => ({
                        ...prev,
                        max_connections_per_download: Number(e.target.value),
                      }))
                    }
                  />
                </label>

                <label className="vtx-field">
                  <span className="vtx-field-label">
                    {t("settings.maxConcurrent")}:{" "}
                    <strong className="vtx-tabular">{draft.max_concurrent_downloads}</strong>
                  </span>
                  <input
                    type="range"
                    min={1}
                    max={16}
                    value={draft.max_concurrent_downloads}
                    onChange={(e) =>
                      setDraft((prev) => ({
                        ...prev,
                        max_concurrent_downloads: Number(e.target.value),
                      }))
                    }
                  />
                </label>

                <label className="vtx-checkbox-label">
                  <input
                    type="checkbox"
                    checked={draft.auto_scale_connections ?? true}
                    onChange={(e) =>
                      setDraft((prev) => ({
                        ...prev,
                        auto_scale_connections: e.target.checked,
                      }))
                    }
                  />
                  <span>{t("settings.autoScaleConnections")}</span>
                </label>

                <label className="vtx-checkbox-label">
                  <input
                    type="checkbox"
                    checked={draft.enable_http2 ?? true}
                    onChange={(e) =>
                      setDraft((prev) => ({
                        ...prev,
                        enable_http2: e.target.checked,
                      }))
                    }
                  />
                  <span>{t("settings.enableHttp2")}</span>
                </label>

                <label className="vtx-checkbox-label">
                  <input
                    type="checkbox"
                    checked={draft.prevent_sleep_while_active}
                    onChange={(e) =>
                      setDraft((prev) => ({
                        ...prev,
                        prevent_sleep_while_active: e.target.checked,
                      }))
                    }
                  />
                  <span>{t("settings.preventSleep")}</span>
                </label>
              </>
            )}

            {tab === "capture" && (
              <>
                <fieldset className="vtx-radio-group">
                  <legend className="vtx-field-label">{t("settings.confirmPolicyLabel")}</legend>
                  <label className="vtx-radio-card">
                    <input
                      type="radio"
                      name="confirmPolicy"
                      checked={!draft.auto_start_handoff}
                      onChange={() => setDraft((prev) => ({ ...prev, auto_start_handoff: false }))}
                    />
                    <span>{t("settings.confirmAlways")}</span>
                  </label>
                  <label className="vtx-radio-card">
                    <input
                      type="radio"
                      name="confirmPolicy"
                      checked={draft.auto_start_handoff}
                      onChange={() => setDraft((prev) => ({ ...prev, auto_start_handoff: true }))}
                    />
                    <span>{t("settings.confirmAutoSafe")}</span>
                  </label>
                </fieldset>

                <label className="vtx-checkbox-label">
                  <input
                    type="checkbox"
                    checked={draft.capture_rules.capture_in_incognito}
                    onChange={(e) =>
                      setDraft((prev) => ({
                        ...prev,
                        capture_rules: {
                          ...prev.capture_rules,
                          capture_in_incognito: e.target.checked,
                        },
                      }))
                    }
                  />
                  <span>{t("settings.captureIncognito")}</span>
                </label>

                <div className="vtx-field">
                  <label htmlFor="vtx-add-host" className="vtx-field-label">
                    {t("settings.excludedHosts")}
                  </label>
                  <div className="vtx-input-group">
                    <input
                      id="vtx-add-host"
                      type="text"
                      className="vtx-input vtx-ltr-isolate"
                      placeholder={t("settings.excludedPlaceholder")}
                      value={hostInput}
                      onChange={(e) => setHostInput(e.target.value)}
                      onKeyDown={(e) => {
                        if (e.key === "Enter") {
                          e.preventDefault();
                          handleAddHost();
                        }
                      }}
                    />
                    <button type="button" className="vtx-btn" onClick={handleAddHost}>
                      <Plus size={14} />
                      <span>{t("settings.addHost")}</span>
                    </button>
                  </div>

                  <div className="vtx-chip-list">
                    {draft.capture_rules.excluded_hosts.map((host) => (
                      <span key={host} className="vtx-chip vtx-ltr-isolate">
                        {host}
                        <button
                          type="button"
                          className="vtx-chip-remove"
                          aria-label={`${t("settings.removeHost")} ${host}`}
                          onClick={() => handleRemoveHost(host)}
                        >
                          <X size={12} />
                        </button>
                      </span>
                    ))}
                  </div>
                </div>
              </>
            )}
          </div>
        </div>

        <footer className="vtx-confirm-footer">
          <div
            role="status"
            aria-live="polite"
            style={{ color: "var(--vtx-status-success)", fontSize: "12px" }}
          >
            {savedNotice && (
              <span style={{ display: "inline-flex", alignItems: "center", gap: "6px" }}>
                <Check size={14} /> {t("settings.savedToast")}
              </span>
            )}
          </div>
          <div className="vtx-confirm-footer-end">
            <button type="button" className="vtx-btn" onClick={onClose}>
              {t("settings.close")}
            </button>
            <button
              type="button"
              className="vtx-btn vtx-btn-primary"
              onClick={() => void handleSubmit()}
            >
              <Check size={14} />
              <span>{t("settings.save")}</span>
            </button>
          </div>
        </footer>
      </div>
    </div>
  );
}
