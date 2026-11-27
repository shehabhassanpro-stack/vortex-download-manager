/**
 * Root Application Shell (`apps/desktop/ui/src/App.tsx` — `P1.5` Creative Overhaul).
 *
 * Unifies:
 * - Compact Standalone Tray Confirmation Popup Mode (`?mode=confirm`) with auto-close when empty
 * - Edge-to-Edge `100vw x 100vh` Windows 11 Mica Workspace with Top Command Bar + Live Telemetry Sparkline
 * - Navigation Rail with Live Status/Category Count Badges + Drive Storage Gauge
 * - `60 FPS` Virtualized Downloads Table + Dockable `512-Bin` Live Inspector Drawer
 * - Flagship Smart Capture Confirmation Modal (Magic-Byte Shield, Burst Quarantine, `350ms` Click Guard)
 * - Native Settings Modal, Diagnostics Modal, Executable Launch Guard, and `6-second` Undo Toast.
 */

import { useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  Activity,
  AlertTriangle,
  Archive,
  CheckCircle2,
  Cpu,
  Download,
  ExternalLink,
  FileText,
  Film,
  HardDrive,
  Image as ImageIcon,
  Layers,
  Moon,
  Music,
  Pause,
  PauseCircle,
  Plus,
  Search,
  Settings,
  ShieldAlert,
  ShieldCheck,
  Sun,
  Undo2,
  X,
} from "lucide-react";
import { vortexApi, type DownloadSummaryDto } from "./bindings/client.js";
import { SmartConfirmModal } from "./features/capture/SmartConfirmModal.js";
import { DiagnosticsPanel } from "./features/diagnostics/DiagnosticsPanel.js";
import { VirtualizedDownloadsTable } from "./features/downloads/VirtualizedDownloadsTable.js";
import { useDownloadsStore } from "./features/downloads/store.js";
import { LiveInspectorDrawer } from "./features/progress/LiveInspectorDrawer.js";
import { SettingsModal } from "./features/settings/SettingsModal.js";
import { ensureI18n } from "./shared/i18n/index.js";
import { formatBytes, formatSpeed } from "./shared/lib/format.js";
import { VortexErrorBoundary } from "./shared/ui/ErrorBoundaries.js";
import { isExecutableFilename } from "./shared/ui/FileTypeBadge.js";
import { SpeedSparkline } from "./shared/ui/SpeedSparkline.js";
import { VortexLogo } from "./shared/ui/VortexLogo.js";
import { DegradedModeBanner } from "./shared/ui/VtxErrorCard.js";

ensureI18n();

type ThemeMode = "dark" | "light";
type ActiveModal = "none" | "settings" | "diagnostics";

function detectConfirmPopupMode(): boolean {
  if (typeof window === "undefined") {
    return false;
  }
  return new URLSearchParams(window.location.search).get("mode") === "confirm";
}

export function App() {
  const { t, i18n } = useTranslation();
  const isConfirmPopupMode = useMemo(() => detectConfirmPopupMode(), []);

  const [theme, setTheme] = useState<ThemeMode>("dark");
  const [activeModal, setActiveModal] = useState<ActiveModal>("none");
  const [manualAddOpen, setManualAddOpen] = useState(false);
  const [droppedUrl, setDroppedUrl] = useState("");
  const [isDraggingOver, setIsDraggingOver] = useState(false);
  const [pendingExecOpen, setPendingExecOpen] = useState<DownloadSummaryDto | null>(null);
  const [sawStagedInPopup, setSawStagedInPopup] = useState(false);
  const searchInputRef = useRef<HTMLInputElement | null>(null);

  const items = useDownloadsStore((s) => s.items);
  const allItems = useDownloadsStore((s) => s.allItems);
  const selectedId = useDownloadsStore((s) => s.selectedId);
  const activeStateFilter = useDownloadsStore((s) => s.activeStateFilter);
  const activeCategoryFilter = useDownloadsStore((s) => s.activeCategoryFilter);
  const searchQuery = useDownloadsStore((s) => s.searchQuery);
  const undoBanner = useDownloadsStore((s) => s.undoBanner);
  const degradedMode = useDownloadsStore((s) => s.degradedMode);
  const progressBinsById = useDownloadsStore((s) => s.progressBinsById);
  const speedHistoryById = useDownloadsStore((s) => s.speedHistoryById);
  const protocolById = useDownloadsStore((s) => s.protocolById);
  const concurrencyModeById = useDownloadsStore((s) => s.concurrencyModeById);
  const globalSpeedHistory = useDownloadsStore((s) => s.globalSpeedHistory);
  const stagedQueue = useDownloadsStore((s) => s.stagedQueue);
  const settings = useDownloadsStore((s) => s.settings);
  const freeDiskBytes = useDownloadsStore((s) => s.freeDiskBytes);
  const inspectorOpen = useDownloadsStore((s) => s.inspectorOpen);

  const refresh = useDownloadsStore((s) => s.refresh);
  const refreshStagedQueue = useDownloadsStore((s) => s.refreshStagedQueue);
  const loadSettings = useDownloadsStore((s) => s.loadSettings);
  const saveSettings = useDownloadsStore((s) => s.saveSettings);
  const setSearchQuery = useDownloadsStore((s) => s.setSearchQuery);
  const setStateFilter = useDownloadsStore((s) => s.setStateFilter);
  const setCategoryFilter = useDownloadsStore((s) => s.setCategoryFilter);
  const selectDownload = useDownloadsStore((s) => s.selectDownload);
  const setInspectorOpen = useDownloadsStore((s) => s.setInspectorOpen);
  const applyProgressBatch = useDownloadsStore((s) => s.applyProgressBatch);
  const togglePauseResumeById = useDownloadsStore((s) => s.togglePauseResumeById);
  const pauseOrResumeSelected = useDownloadsStore((s) => s.pauseOrResumeSelected);
  const pauseAll = useDownloadsStore((s) => s.pauseAll);
  const removeWithUndo = useDownloadsStore((s) => s.removeWithUndo);
  const undoLastRemove = useDownloadsStore((s) => s.undoLastRemove);
  const discardStaged = useDownloadsStore((s) => s.discardStaged);
  const discardAllStaged = useDownloadsStore((s) => s.discardAllStaged);
  const blockHostAndDiscard = useDownloadsStore((s) => s.blockHostAndDiscard);

  const isAr = i18n.language === "ar";

  useEffect(() => {
    document.documentElement.lang = i18n.language;
    document.documentElement.dir = isAr ? "rtl" : "ltr";
    document.documentElement.setAttribute("data-theme", theme);
  }, [i18n.language, isAr, theme]);

  // Initial pull + live Tauri event subscriptions (zero cold-boot race!)
  useEffect(() => {
    void refreshStagedQueue();
    void loadSettings();
    if (!isConfirmPopupMode) {
      void refresh();
    }

    let unlistenProgress: (() => void) | undefined;
    let unlistenDomain: (() => void) | undefined;
    let unlistenOpen: (() => void) | undefined;
    let unlistenStaged: (() => void) | undefined;

    void vortexApi
      .onStagedDownloadsChanged(() => {
        void refreshStagedQueue();
      })
      .then((fn) => {
        unlistenStaged = fn;
      });

    if (!isConfirmPopupMode) {
      void vortexApi
        .onProgressBatch((batch) => applyProgressBatch(batch))
        .then((fn) => {
          unlistenProgress = fn;
        });
      void vortexApi
        .onDomainEvent(() => void refresh())
        .then((fn) => {
          unlistenDomain = fn;
        });
      void vortexApi
        .onOpenUrl((incoming) => {
          if (incoming.startsWith("http://") || incoming.startsWith("https://")) {
            setDroppedUrl(incoming);
            setManualAddOpen(true);
          } else if (incoming.startsWith("vortex://staged/")) {
            void refreshStagedQueue();
          } else {
            void refresh();
          }
        })
        .then((fn) => {
          unlistenOpen = fn;
        });
    }

    return () => {
      unlistenProgress?.();
      unlistenDomain?.();
      unlistenOpen?.();
      unlistenStaged?.();
    };
  }, [applyProgressBatch, isConfirmPopupMode, loadSettings, refresh, refreshStagedQueue]);

  // Sync theme/language from persisted settings on load
  useEffect(() => {
    if (settings === null) return;
    if (settings.theme === "light" || settings.theme === "dark") {
      setTheme(settings.theme);
    }
    if (settings.language !== i18n.language) {
      void i18n.changeLanguage(settings.language);
    }
  }, [settings?.theme, settings?.language]);

  // Auto-close compact Tray Confirmation Window cleanly once all staged captures are handled
  useEffect(() => {
    if (!isConfirmPopupMode) return;
    if (stagedQueue.length > 0) {
      setSawStagedInPopup(true);
    } else if (sawStagedInPopup) {
      void vortexApi.closeCurrentWindow();
    }
  }, [isConfirmPopupMode, sawStagedInPopup, stagedQueue.length]);

  // Global Keyboard Shortcuts (`Ctrl+N`, `Ctrl+F`, `Space`, `Delete`, `Esc`)
  useEffect(() => {
    if (isConfirmPopupMode) return;
    const onKeyDown = (ev: KeyboardEvent): void => {
      const targetTag = (ev.target as HTMLElement | null)?.tagName;
      const inInput = targetTag === "INPUT" || targetTag === "TEXTAREA";

      if (ev.ctrlKey && ev.key.toLowerCase() === "n") {
        ev.preventDefault();
        setManualAddOpen(true);
        return;
      }
      if (ev.ctrlKey && ev.key.toLowerCase() === "f") {
        ev.preventDefault();
        setActiveModal("none");
        searchInputRef.current?.focus();
        return;
      }
      if (ev.key === "Escape") {
        if (pendingExecOpen !== null) {
          setPendingExecOpen(null);
        } else if (activeModal !== "none") {
          setActiveModal("none");
        }
        return;
      }
      if (inInput || manualAddOpen || stagedQueue.length > 0 || activeModal !== "none") return;

      if (ev.key === " ") {
        ev.preventDefault();
        void pauseOrResumeSelected();
      } else if (ev.key === "Delete" && selectedId !== null) {
        ev.preventDefault();
        void removeWithUndo(selectedId);
      }
    };

    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [
    activeModal,
    isConfirmPopupMode,
    manualAddOpen,
    pauseOrResumeSelected,
    pendingExecOpen,
    removeWithUndo,
    selectedId,
    stagedQueue.length,
  ]);

  // Live sidebar badge counters & global aggregate telemetry
  const counts = useMemo(() => {
    const byState: Record<string, number> = {
      all: allItems.length,
      downloading: 0,
      completed: 0,
      paused: 0,
      needs_attention: 0,
    };
    const byCat: Record<string, number> = {
      video: 0,
      audio: 0,
      compressed: 0,
      programs: 0,
      documents: 0,
      images: 0,
    };
    let totalActiveBps = 0;

    for (const d of allItems) {
      if (d.state_slug === "downloading" || d.state_slug === "probing") {
        byState["downloading"] = (byState["downloading"] ?? 0) + 1;
        totalActiveBps += d.speed_bps;
      } else if (d.state_slug === "completed") {
        byState["completed"] = (byState["completed"] ?? 0) + 1;
      } else if (d.state_slug === "paused" || d.state_slug === "queued") {
        byState["paused"] = (byState["paused"] ?? 0) + 1;
      } else if (d.state_slug === "needs_attention" || d.state_slug === "failed") {
        byState["needs_attention"] = (byState["needs_attention"] ?? 0) + 1;
      }
      if (d.category_id in byCat) {
        byCat[d.category_id] = (byCat[d.category_id] ?? 0) + 1;
      }
    }

    return { byState, byCat, totalActiveBps };
  }, [allItems]);

  const activeCount = counts.byState["downloading"] ?? 0;
  const selectedItem = useMemo(
    () => items.find((i) => i.id === selectedId) ?? null,
    [items, selectedId],
  );

  const handleOpenCompletedFile = (item: DownloadSummaryDto): void => {
    if (isExecutableFilename(item.file_name)) {
      setPendingExecOpen(item);
      return;
    }
    void vortexApi.openDownloadedFile(item.id);
  };

  // Standalone Compact Tray Confirmation Popup Window (`?mode=confirm` — `P1.5-T04`)
  if (isConfirmPopupMode) {
    return (
      <VortexErrorBoundary
        featureName="StandaloneConfirmShell"
        isRoot
        onReportError={(comp, msg) => vortexApi.logFrontendError(comp, msg)}
      >
        <SmartConfirmModal
          stagedQueue={stagedQueue}
          manualMode={stagedQueue.length === 0}
          defaultSaveDir={settings?.default_download_dir ?? ""}
          standaloneWindow
          onConfirmed={() => {
            void refreshStagedQueue();
          }}
          onDiscardStaged={discardStaged}
          onDiscardAllStaged={discardAllStaged}
          onBlockHost={blockHostAndDiscard}
          onCloseManual={() => void vortexApi.closeCurrentWindow()}
        />
      </VortexErrorBoundary>
    );
  }

  const showConfirmModal = manualAddOpen || stagedQueue.length > 0;

  return (
    <VortexErrorBoundary
      featureName="RootShell"
      isRoot
      onReportError={(comp, msg) => vortexApi.logFrontendError(comp, msg)}
    >
      <div
        className="vtx-app-shell"
        onDragOver={(ev) => {
          ev.preventDefault();
          setIsDraggingOver(true);
        }}
        onDragLeave={(ev) => {
          if (ev.currentTarget === ev.target) {
            setIsDraggingOver(false);
          }
        }}
        onDrop={(ev) => {
          ev.preventDefault();
          setIsDraggingOver(false);
          const text =
            ev.dataTransfer.getData("text/uri-list") || ev.dataTransfer.getData("text/plain");
          const firstUrl = text
            .split(/\r?\n/)
            .map((l) => l.trim())
            .find((l) => l.startsWith("http://") || l.startsWith("https://"));
          if (firstUrl !== undefined) {
            setDroppedUrl(firstUrl);
            setManualAddOpen(true);
          }
        }}
      >
        <DegradedModeBanner visible={degradedMode} onRetry={() => void refresh()} />

        {/* 1. Edge-to-Edge Top Command Bar (`56px`) */}
        <header className="vtx-command-bar">
          <div className="vtx-brand-cluster">
            <VortexLogo size={32} spinning={activeCount > 0} />
            <div className="vtx-brand-text">
              <strong className="vtx-brand-name">{t("app.title")}</strong>
              <span className="vtx-brand-sub">{t("app.brandSub")}</span>
            </div>

            {/* Live Global Telemetry Pill */}
            <div
              className={`vtx-telemetry-pill ${activeCount > 0 ? "vtx-telemetry-active" : ""}`}
              aria-live="off"
            >
              <span className="vtx-telemetry-speed vtx-tabular">
                {activeCount > 0 ? formatSpeed(counts.totalActiveBps) : t("app.idle")}
              </span>
              <SpeedSparkline samples={globalSpeedHistory} />
            </div>
          </div>

          <div className="vtx-command-actions">
            <div className="vtx-search-box">
              <Search size={15} className="vtx-search-icon" aria-hidden="true" />
              <input
                ref={searchInputRef}
                type="search"
                className="vtx-search-input"
                value={searchQuery}
                onChange={(ev) => void setSearchQuery(ev.target.value)}
                placeholder={t("toolbar.searchPlaceholder")}
                aria-label={t("toolbar.searchPlaceholder")}
              />
            </div>

            <button
              type="button"
              className="vtx-btn vtx-btn-primary"
              data-testid="btn-add-url"
              onClick={() => {
                setDroppedUrl("");
                setManualAddOpen(true);
              }}
            >
              <Plus size={15} />
              <span>{t("toolbar.addUrl")}</span>
            </button>

            {activeCount > 0 && (
              <button
                type="button"
                className="vtx-btn"
                title={t("toolbar.pauseAll")}
                onClick={() => void pauseAll()}
              >
                <PauseCircle size={15} />
                <span>{t("toolbar.pauseAll")}</span>
              </button>
            )}

            <button
              type="button"
              className={`vtx-icon-btn ${inspectorOpen ? "vtx-icon-btn-active" : ""}`}
              title={t("nav.progress")}
              aria-label={t("nav.progress")}
              onClick={() => setInspectorOpen(!inspectorOpen)}
            >
              <Activity size={16} />
            </button>

            <button
              type="button"
              className={`vtx-icon-btn ${activeModal === "diagnostics" ? "vtx-icon-btn-active" : ""}`}
              title={t("nav.diagnostics")}
              aria-label={t("nav.diagnostics")}
              onClick={() =>
                setActiveModal((prev) => (prev === "diagnostics" ? "none" : "diagnostics"))
              }
            >
              <ShieldCheck size={16} />
            </button>

            <button
              type="button"
              className={`vtx-icon-btn ${activeModal === "settings" ? "vtx-icon-btn-active" : ""}`}
              title={t("nav.settings")}
              aria-label={t("nav.settings")}
              onClick={() => setActiveModal((prev) => (prev === "settings" ? "none" : "settings"))}
            >
              <Settings size={16} />
            </button>

            <button
              type="button"
              className="vtx-icon-btn"
              title={
                theme === "dark" ? t("toolbar.switchThemeLight") : t("toolbar.switchThemeDark")
              }
              aria-label={
                theme === "dark" ? t("toolbar.switchThemeLight") : t("toolbar.switchThemeDark")
              }
              onClick={() => setTheme((prev) => (prev === "dark" ? "light" : "dark"))}
            >
              {theme === "dark" ? <Sun size={16} /> : <Moon size={16} />}
            </button>

            <button
              type="button"
              className="vtx-btn vtx-lang-btn"
              onClick={() => void i18n.changeLanguage(isAr ? "en" : "ar")}
            >
              {t("toolbar.switchLang")}
            </button>
          </div>
        </header>

        {/* 2. Workspace Body (Start Navigation Rail + Flex-1 Stage) */}
        <div className="vtx-workspace-body">
          <aside className="vtx-nav-rail" aria-label={t("nav.categories")}>
            <div className="vtx-nav-section-label">{t("nav.statusSection")}</div>

            {(
              [
                [null, t("nav.all"), Layers, counts.byState["all"] ?? 0],
                ["downloading", t("nav.downloading"), Download, counts.byState["downloading"] ?? 0],
                ["completed", t("nav.completed"), CheckCircle2, counts.byState["completed"] ?? 0],
                ["paused", t("nav.paused"), Pause, counts.byState["paused"] ?? 0],
                [
                  "needs_attention",
                  t("nav.needs_attention"),
                  AlertTriangle,
                  counts.byState["needs_attention"] ?? 0,
                ],
              ] as const
            ).map(([slug, label, Icon, count]) => {
              const active = activeStateFilter === slug && activeCategoryFilter === null;
              return (
                <button
                  key={slug ?? "all"}
                  type="button"
                  className={`vtx-nav-item ${active ? "vtx-nav-item-active" : ""}`}
                  onClick={() => {
                    void setCategoryFilter(null);
                    void setStateFilter(slug);
                  }}
                >
                  <span className="vtx-nav-label">
                    <Icon size={15} />
                    <span>{label}</span>
                  </span>
                  <span className="vtx-nav-count vtx-tabular">{count}</span>
                </button>
              );
            })}

            <div className="vtx-nav-divider" />
            <div className="vtx-nav-section-label">{t("nav.categories")}</div>

            {(
              [
                ["video", t("nav.cat_video"), Film],
                ["audio", t("nav.cat_audio"), Music],
                ["compressed", t("nav.cat_compressed"), Archive],
                ["programs", t("nav.cat_programs"), Cpu],
                ["documents", t("nav.cat_documents"), FileText],
                ["images", t("nav.cat_images"), ImageIcon],
              ] as const
            ).map(([catId, label, Icon]) => {
              const active = activeCategoryFilter === catId;
              const count = counts.byCat[catId] ?? 0;
              return (
                <button
                  key={catId}
                  type="button"
                  className={`vtx-nav-item ${active ? "vtx-nav-item-active" : ""}`}
                  onClick={() => {
                    void setStateFilter(null);
                    void setCategoryFilter(active ? null : catId);
                  }}
                >
                  <span className="vtx-nav-label">
                    <Icon size={15} />
                    <span>{label}</span>
                  </span>
                  {count > 0 && <span className="vtx-nav-count vtx-tabular">{count}</span>}
                </button>
              );
            })}

            {/* Bottom Drive Storage Gauge */}
            <div className="vtx-storage-card">
              <div className="vtx-storage-head">
                <HardDrive size={14} />
                <span>{t("nav.storageLabel")}</span>
              </div>
              <strong className="vtx-storage-value vtx-tabular">
                {freeDiskBytes !== null ? formatBytes(freeDiskBytes) : "—"}
              </strong>
              {settings !== null && (
                <div
                  className="vtx-storage-path vtx-ltr-isolate"
                  title={settings.default_download_dir}
                >
                  {settings.default_download_dir}
                </div>
              )}
            </div>
          </aside>

          {/* Main Downloads Stage */}
          <main className="vtx-main-stage" aria-label={t("nav.main")}>
            <VortexErrorBoundary
              featureName="MainDownloadsWindow"
              onReportError={(comp, msg) => vortexApi.logFrontendError(comp, msg)}
            >
              <div className="vtx-stage-content">
                <VirtualizedDownloadsTable
                  items={items}
                  selectedId={selectedId}
                  onSelect={(id) => selectDownload(id)}
                  onInspectProgress={(id) => {
                    selectDownload(id);
                    setInspectorOpen(true);
                  }}
                  onTogglePauseResume={(item) => void togglePauseResumeById(item.id)}
                  onRevealInFolder={(id) => void vortexApi.revealInFolder(id)}
                  onOpenFile={handleOpenCompletedFile}
                  onDelete={(id) => void removeWithUndo(id)}
                  onAddFirstUrl={() => {
                    setDroppedUrl("");
                    setManualAddOpen(true);
                  }}
                />
              </div>

              {inspectorOpen && selectedItem !== null && (
                <VortexErrorBoundary
                  featureName="LiveInspectorDrawer"
                  onReportError={(comp, msg) => vortexApi.logFrontendError(comp, msg)}
                >
                  <LiveInspectorDrawer
                    item={selectedItem}
                    {...(progressBinsById[selectedItem.id] !== undefined
                      ? { liveBins: progressBinsById[selectedItem.id] }
                      : {})}
                    {...(speedHistoryById[selectedItem.id] !== undefined
                      ? { speedHistory: speedHistoryById[selectedItem.id] }
                      : {})}
                    {...(protocolById[selectedItem.id] !== undefined
                      ? { protocol: protocolById[selectedItem.id] }
                      : {})}
                    {...(concurrencyModeById[selectedItem.id] !== undefined
                      ? { concurrencyMode: concurrencyModeById[selectedItem.id] }
                      : {})}
                    theme={theme}
                    onTogglePauseResume={(item) => void togglePauseResumeById(item.id)}
                    onRevealInFolder={(id) => void vortexApi.revealInFolder(id)}
                    onOpenFile={handleOpenCompletedFile}
                    onClose={() => setInspectorOpen(false)}
                  />
                </VortexErrorBoundary>
              )}
            </VortexErrorBoundary>
          </main>
        </div>

        {/* 3. Bottom Status Bar (`32px`) */}
        <footer className="vtx-status-bar">
          <div className="vtx-status-group">
            <span className="vtx-pulse-dot" aria-hidden="true" />
            <span>{t("app.ready")}</span>
            <span className="vtx-status-sep">·</span>
            <span className="vtx-tabular">{t("toolbar.itemsCount", { count: items.length })}</span>
          </div>

          <div className="vtx-status-group">
            <span className="vtx-kbd-hint vtx-ltr-isolate">Ctrl+N</span>
            <span className="vtx-kbd-hint vtx-ltr-isolate">Ctrl+F</span>
            <span className="vtx-kbd-hint vtx-ltr-isolate">Space</span>
          </div>
        </footer>

        {/* 4. Floating 6-Second Undo Delete Toast */}
        {undoBanner !== null && (
          <div role="status" aria-live="polite" className="vtx-undo-toast">
            <span>{t("toolbar.undoDeleted", { name: undoBanner.fileName })}</span>
            <button
              type="button"
              className="vtx-btn vtx-btn-primary"
              onClick={() => void undoLastRemove()}
            >
              <Undo2 size={14} />
              <span>{t("toolbar.undoAction")}</span>
            </button>
          </div>
        )}

        {/* 5. Drag & Drop Glass Portal */}
        {isDraggingOver && (
          <div role="status" className="vtx-drop-portal">
            <div className="vtx-drop-portal-card">
              <VortexLogo size={52} spinning />
              <strong>{t("toolbar.dropHint")}</strong>
            </div>
          </div>
        )}

        {/* 6. Flagship Smart Capture Confirmation & Manual Add Modal */}
        {showConfirmModal && (
          <VortexErrorBoundary
            featureName="SmartConfirmModal"
            onReportError={(comp, msg) => vortexApi.logFrontendError(comp, msg)}
          >
            <SmartConfirmModal
              stagedQueue={stagedQueue}
              manualMode={manualAddOpen}
              initialUrl={droppedUrl}
              defaultSaveDir={settings?.default_download_dir ?? ""}
              onConfirmed={() => {
                setDroppedUrl("");
                setManualAddOpen(false);
                void refresh();
                void refreshStagedQueue();
              }}
              onDiscardStaged={discardStaged}
              onDiscardAllStaged={discardAllStaged}
              onBlockHost={blockHostAndDiscard}
              onJumpToExisting={(existingId) => {
                selectDownload(existingId);
                setInspectorOpen(true);
              }}
              onCloseManual={() => {
                setDroppedUrl("");
                setManualAddOpen(false);
              }}
            />
          </VortexErrorBoundary>
        )}

        {/* 7. Native Settings Modal */}
        {activeModal === "settings" && settings !== null && (
          <VortexErrorBoundary
            featureName="SettingsModal"
            onReportError={(comp, msg) => vortexApi.logFrontendError(comp, msg)}
          >
            <SettingsModal
              config={settings}
              theme={theme}
              onChangeTheme={setTheme}
              onChangeLanguage={(lang) => void i18n.changeLanguage(lang)}
              onSave={async (next) => {
                await saveSettings(next);
              }}
              onClose={() => setActiveModal("none")}
            />
          </VortexErrorBoundary>
        )}

        {/* 8. Diagnostics & Live Logs Modal */}
        {activeModal === "diagnostics" && (
          <VortexErrorBoundary
            featureName="DiagnosticsPanel"
            onReportError={(comp, msg) => vortexApi.logFrontendError(comp, msg)}
          >
            <DiagnosticsPanel onClose={() => setActiveModal("none")} />
          </VortexErrorBoundary>
        )}

        {/* 9. Safe Executable Launch Confirmation Guard (`TH-23`) */}
        {pendingExecOpen !== null && (
          <div className="vtx-modal-scrim">
            <div
              role="alertdialog"
              aria-modal="true"
              aria-label={t("confirm.execOpenConfirmTitle")}
              className="vtx-exec-guard-card"
            >
              <header className="vtx-confirm-header">
                <div className="vtx-confirm-title-group">
                  <ShieldAlert size={20} style={{ color: "var(--vtx-status-warning)" }} />
                  <h2 className="vtx-confirm-heading">{t("confirm.execOpenConfirmTitle")}</h2>
                </div>
                <button
                  type="button"
                  className="vtx-icon-btn"
                  aria-label={t("confirm.cancel")}
                  onClick={() => setPendingExecOpen(null)}
                >
                  <X size={16} />
                </button>
              </header>
              <p className="vtx-exec-guard-body">
                {t("confirm.execOpenConfirmBody", { name: pendingExecOpen.file_name })}
              </p>
              <footer className="vtx-confirm-footer-end">
                <button type="button" className="vtx-btn" onClick={() => setPendingExecOpen(null)}>
                  {t("confirm.cancel")}
                </button>
                <button
                  type="button"
                  className="vtx-btn vtx-btn-primary"
                  onClick={() => {
                    const targetId = pendingExecOpen.id;
                    setPendingExecOpen(null);
                    void vortexApi.openDownloadedFile(targetId);
                  }}
                >
                  <ExternalLink size={14} />
                  <span>{t("confirm.execOpenProceed")}</span>
                </button>
              </footer>
            </div>
          </div>
        )}
      </div>
    </VortexErrorBoundary>
  );
}
