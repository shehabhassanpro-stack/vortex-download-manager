/**
 * Zustand Store for Vortex Desktop UI (`apps/desktop/ui/src/features/downloads/store.ts` — `P1-T32` & `P1.5`).
 *
 * Applies `<= 10Hz` `ProgressSnapshotDto[]` delta batches with `O(batch)` structural sharing,
 * manages the pull-based `stagedQueue` (zero cold-boot race!), live global telemetry sparkline,
 * sidebar category/status counts, settings, and target-drive free space.
 */

import { create } from "zustand";
import {
  vortexApi,
  type AppConfigDto,
  type DownloadFilterDto,
  type DownloadSummaryDto,
  type ProgressSnapshotDto,
  type StagedDownloadDto,
} from "../../bindings/client.js";

export interface UndoState {
  readonly id: string;
  readonly fileName: string;
  readonly expiresAtMs: number;
}

interface DownloadsState {
  readonly items: readonly DownloadSummaryDto[];
  readonly allItems: readonly DownloadSummaryDto[];
  readonly selectedId: string | null;
  readonly activeStateFilter: string | null;
  readonly activeCategoryFilter: string | null;
  readonly searchQuery: string;
  readonly undoBanner: UndoState | null;
  readonly degradedMode: boolean;
  readonly progressBinsById: Readonly<Record<string, readonly number[]>>;
  readonly speedHistoryById: Readonly<Record<string, readonly number[]>>;
  readonly protocolById: Readonly<Record<string, string>>;
  readonly concurrencyModeById: Readonly<Record<string, string>>;
  readonly globalSpeedHistory: readonly number[];
  readonly stagedQueue: readonly StagedDownloadDto[];
  readonly settings: AppConfigDto | null;
  readonly freeDiskBytes: number | null;
  readonly inspectorOpen: boolean;

  refresh: () => Promise<void>;
  refreshStagedQueue: () => Promise<void>;
  loadSettings: () => Promise<void>;
  saveSettings: (next: AppConfigDto) => Promise<AppConfigDto>;
  refreshFreeDisk: (dir?: string) => Promise<void>;
  setSearchQuery: (q: string) => Promise<void>;
  setStateFilter: (stateSlug: string | null) => Promise<void>;
  setCategoryFilter: (catId: string | null) => Promise<void>;
  selectDownload: (id: string | null) => void;
  setInspectorOpen: (open: boolean) => void;
  applyProgressBatch: (batch: readonly ProgressSnapshotDto[]) => void;
  togglePauseResumeById: (id: string) => Promise<void>;
  pauseOrResumeSelected: () => Promise<void>;
  pauseAll: () => Promise<void>;
  removeWithUndo: (id: string) => Promise<void>;
  undoLastRemove: () => Promise<void>;
  discardStaged: (stagedId: string) => Promise<void>;
  discardAllStaged: () => Promise<void>;
  blockHostAndDiscard: (stagedId: string, host: string) => Promise<void>;
  seedVirtualizationRows: (count: number) => void;
}

export const useDownloadsStore = create<DownloadsState>((set, get) => ({
  items: [],
  allItems: [],
  selectedId: null,
  activeStateFilter: null,
  activeCategoryFilter: null,
  searchQuery: "",
  undoBanner: null,
  degradedMode: false,
  progressBinsById: {},
  speedHistoryById: {},
  protocolById: {},
  concurrencyModeById: {},
  globalSpeedHistory: [0, 0, 0, 0, 0, 0, 0, 0],
  stagedQueue: [],
  settings: null,
  freeDiskBytes: null,
  inspectorOpen: false,

  async refresh(): Promise<void> {
    try {
      const { activeStateFilter, activeCategoryFilter, searchQuery } = get();
      const allPromise = vortexApi.listDownloads();

      let filteredRows: DownloadSummaryDto[];
      let allRows: DownloadSummaryDto[];

      if (searchQuery.trim().length > 0) {
        [filteredRows, allRows] = await Promise.all([
          vortexApi.searchDownloads(searchQuery),
          allPromise,
        ]);
      } else if (activeStateFilter !== null || activeCategoryFilter !== null) {
        const filter: DownloadFilterDto = {
          ...(activeStateFilter !== null ? { state_slug: activeStateFilter } : {}),
          ...(activeCategoryFilter !== null ? { category_id: activeCategoryFilter } : {}),
        };
        [filteredRows, allRows] = await Promise.all([vortexApi.listDownloads(filter), allPromise]);
      } else {
        allRows = await allPromise;
        filteredRows = allRows;
      }

      set((prev) => ({
        items: filteredRows,
        allItems: allRows,
        selectedId:
          prev.selectedId !== null && filteredRows.some((r) => r.id === prev.selectedId)
            ? prev.selectedId
            : (filteredRows[0]?.id ?? null),
        degradedMode: false,
      }));
    } catch {
      set({ degradedMode: true });
    }
  },

  async refreshStagedQueue(): Promise<void> {
    try {
      const queue = await vortexApi.listStagedDownloads();
      set({ stagedQueue: queue });
    } catch {
      // Keep current queue if core is momentarily reconnecting
    }
  },

  async loadSettings(): Promise<void> {
    try {
      const cfg = await vortexApi.getSettings();
      set({ settings: cfg });
      await get().refreshFreeDisk(cfg.default_download_dir);
    } catch {
      // Ignore during offline boot
    }
  },

  async saveSettings(next: AppConfigDto): Promise<AppConfigDto> {
    const saved = await vortexApi.updateSettings(next);
    set({ settings: saved });
    await get().refreshFreeDisk(saved.default_download_dir);
    return saved;
  },

  async refreshFreeDisk(dir?: string): Promise<void> {
    const target = dir ?? get().settings?.default_download_dir;
    if (target === undefined || target.length === 0) return;
    try {
      const bytes = await vortexApi.getFreeDiskBytes(target);
      set({ freeDiskBytes: bytes });
    } catch {
      // Non-fatal
    }
  },

  async setSearchQuery(q: string): Promise<void> {
    set({ searchQuery: q });
    await get().refresh();
  },

  async setStateFilter(stateSlug: string | null): Promise<void> {
    set({ activeStateFilter: stateSlug });
    await get().refresh();
  },

  async setCategoryFilter(catId: string | null): Promise<void> {
    set({ activeCategoryFilter: catId });
    await get().refresh();
  },

  selectDownload(id: string | null): void {
    set({ selectedId: id });
  },

  setInspectorOpen(open: boolean): void {
    set({ inspectorOpen: open });
  },

  applyProgressBatch(batch: readonly ProgressSnapshotDto[]): void {
    if (batch.length === 0) {
      return;
    }
    const byId = new Map<string, ProgressSnapshotDto>();
    let totalActiveBps = 0;
    for (const snap of batch) {
      byId.set(snap.download_id, snap);
      if (snap.state_slug === "downloading") {
        totalActiveBps += snap.speed_fast_bps;
      }
    }

    set((prev) => {
      const nextBins = { ...prev.progressBinsById };
      const nextHistory = { ...prev.speedHistoryById };
      const nextProtocol = { ...prev.protocolById };
      const nextMode = { ...prev.concurrencyModeById };
      for (const snap of batch) {
        nextBins[snap.download_id] = snap.bins;
        const hist = prev.speedHistoryById[snap.download_id] ?? [];
        nextHistory[snap.download_id] = [...hist.slice(-59), snap.speed_fast_bps];
        if (snap.protocol !== undefined && snap.protocol !== null) {
          nextProtocol[snap.download_id] = snap.protocol;
        }
        if (snap.concurrency_mode !== undefined && snap.concurrency_mode !== null) {
          nextMode[snap.download_id] = snap.concurrency_mode;
        }
      }

      const patchList = (list: readonly DownloadSummaryDto[]): DownloadSummaryDto[] =>
        list.map((item) => {
          const snap = byId.get(item.id);
          if (snap === undefined) {
            return item;
          }
          return {
            ...item,
            state_slug: snap.state_slug,
            downloaded_bytes: snap.downloaded_bytes,
            total_size: snap.total_size ?? item.total_size,
            speed_bps: snap.speed_fast_bps,
            eta_secs: snap.eta_secs,
            active_workers: snap.active_workers,
          };
        });

      return {
        items: patchList(prev.items),
        allItems: patchList(prev.allItems),
        progressBinsById: nextBins,
        speedHistoryById: nextHistory,
        protocolById: nextProtocol,
        concurrencyModeById: nextMode,
        globalSpeedHistory: [...prev.globalSpeedHistory.slice(-23), totalActiveBps],
      };
    });
  },

  async togglePauseResumeById(id: string): Promise<void> {
    const target = get().items.find((i) => i.id === id) ?? get().allItems.find((i) => i.id === id);
    if (target === undefined) return;

    set({ selectedId: id });
    if (target.state_slug === "downloading" || target.state_slug === "probing") {
      await vortexApi.pauseDownload(id);
    } else if (target.state_slug !== "completed") {
      await vortexApi.resumeDownload(id);
    }
    await get().refresh();
  },

  async pauseOrResumeSelected(): Promise<void> {
    const { selectedId } = get();
    if (selectedId === null) return;
    await get().togglePauseResumeById(selectedId);
  },

  async pauseAll(): Promise<void> {
    await vortexApi.pauseAll();
    await get().refresh();
  },

  async removeWithUndo(id: string): Promise<void> {
    const existing =
      get().items.find((i) => i.id === id) ?? get().allItems.find((i) => i.id === id);
    if (existing === undefined) return;

    await vortexApi.removeDownload(id, false);
    const expiresAtMs = Date.now() + 6_000;
    set((prev) => ({
      items: prev.items.filter((i) => i.id !== id),
      allItems: prev.allItems.filter((i) => i.id !== id),
      undoBanner: { id, fileName: existing.file_name, expiresAtMs },
    }));

    setTimeout(() => {
      const current = get().undoBanner;
      if (current !== null && current.id === id) {
        set({ undoBanner: null });
      }
    }, 6_000);
  },

  async undoLastRemove(): Promise<void> {
    const banner = get().undoBanner;
    if (banner === null) return;
    await vortexApi.undoRemoveDownload(banner.id);
    set({ undoBanner: null });
    await get().refresh();
  },

  async discardStaged(stagedId: string): Promise<void> {
    await vortexApi.discardStagedDownload(stagedId);
    set((prev) => ({
      stagedQueue: prev.stagedQueue.filter((s) => s.staged_id !== stagedId),
    }));
  },

  async discardAllStaged(): Promise<void> {
    await vortexApi.discardAllStagedDownloads();
    set({ stagedQueue: [] });
  },

  async blockHostAndDiscard(stagedId: string, host: string): Promise<void> {
    const updatedCfg = await vortexApi.blockCaptureHost(stagedId, host);
    set({ settings: updatedCfg });
    await get().refreshStagedQueue();
  },

  seedVirtualizationRows(count: number): void {
    const generated: DownloadSummaryDto[] = Array.from({ length: count }, (_, idx) => ({
      id: `0192f3a4-0000-7000-8000-${String(idx).padStart(12, "0")}`,
      file_name: `dataset-shard-${String(idx + 1).padStart(5, "0")}.tar.zst`,
      save_dir: "D:/Downloads/Compressed",
      display_url: `https://mirror.example.org/shards/shard-${idx + 1}.tar.zst`,
      total_size: 104_857_600,
      downloaded_bytes: (idx % 100) * 1_048_576,
      state_slug: idx % 7 === 0 ? "downloading" : "completed",
      speed_bps: idx % 7 === 0 ? 18_874_368 : 0,
      eta_secs: idx % 7 === 0 ? 4 : null,
      active_workers: idx % 7 === 0 ? 8 : 0,
      supports_range: true,
      category_id: "compressed",
      queue_id: "main",
      priority: "Normal",
      created_at_ms: 1_764_000_000_000 + idx,
      completed_at_ms: idx % 7 === 0 ? null : 1_764_000_050_000,
      error: null,
    }));
    set({ items: generated, allItems: generated, selectedId: generated[0]?.id ?? null });
  },
}));
