import { useEffect, useMemo, useRef, useState, useSyncExternalStore } from 'react';
import { listen } from '@tauri-apps/api/event';
import { invoke } from '@tauri-apps/api/core';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { BatchTaskCommand, displayTaskName, DownloadQueue } from './components/DownloadQueue';
import { NewDownloadDialog } from './components/NewDownloadDialog';
import { selectedResourceIds, type DownloadDraftItem } from './components/useDownloadDraftBatch';
import { ParseConfirmDialog } from './components/ParseConfirmDialog';
import { DuplicateDownloadDialog } from './components/DuplicateDownloadDialog';
import { RemoveTaskDialog } from './components/RemoveTaskDialog';
import { TaskDetailsDialog } from './components/TaskDetailsDialog';
import type { CloudManagerClient } from './components/CloudManager';
import { CloudWorkspace, type CloudWorkspacePage } from './components/CloudWorkspace';
import { CloudNavigation } from './components/CloudNavigation';
import { SettingsPanel } from './components/SettingsPanel';
import { AboutPanel } from './components/AboutPanel';
import { EngineStatusToast } from './components/EngineStatusBanner';
import { cancelDownloadFile, checkForAppUpdate, configurePrivacyDownload, confirmCloudShareDownload, confirmDownload, defaultSettings, deleteTasks, discardPreparedDownload, getDownloadEngineState, getPrivacyTaskVisibility, getSettings, isTauriRuntime, listAiModels, listPersistedTasks, lockPrivacyTasks, openExternalUrl, openTaskDirectory, openTaskFile, parseDownload, playTaskVideos, resolveDuplicateDownload, retryDownloadEngine, runTaskAction, scanDownloadInputs, selectDownloadFiles, setTaskPrivacy, syncMainWindowIcon, takeExternalLinks, unlockPrivacyTasks, updateAiSettings, updateSettings } from './lib/api';
import * as cloudApi from './lib/cloudApi';
import { createTaskUpdateMerger, subscribeToDownloadEvents, type TaskSnapshotScope } from './lib/downloadEvents';
import { isPrivateTask, normalizeQueueTask, normalizeQueueTasks } from './lib/queueTask';
import { sendDownloadNotification, shouldNotifyTask } from './lib/downloadNotifications';
import { AiSettingsInput, AppSettings, AppUpdate, DownloadEngineState, DuplicateDownload, ParseResult, PrivacyTaskVisibility, QueueTask, QueueTaskInput, TaskActionResult, TaskCommand } from './lib/types';
import { managerViewFromLauncherPayload, ManagerView } from './lib/managerNavigation';
import darkMintIcon from './assets/launcher/dark_mint.svg';
import mintWhiteIcon from './assets/launcher/mint_white.svg';
import { getResolvedTheme, getResolvedThemeId, subscribeTheme } from './lib/theme';
import type { CloudAuthorization, ProviderDescriptor } from './lib/cloudTypes';
import { installWindowResizePerformanceMode } from './lib/windowResizePerformance';
import { connectWindowInteraction, createWindowInteraction, supportsNativeWindowDrag, type WindowInteractionSnapshot } from './lib/windowInteraction';
import { applyStoredTaskOrder, mergeVisibleTaskOrder } from './lib/taskOrder';
import { applyMainWindowDpiSizing } from './lib/mainWindow';
import { settleTaskOperations } from './lib/taskBatch';
import { t, useTranslation } from './i18n';

type TaskListTab = 'all' | 'active' | 'completed' | 'failed';
type TaskSortMode = 'manual' | 'default-all' | 'created-desc' | 'created-asc' | 'finished-desc' | 'finished-asc' | 'progress-asc' | 'progress-desc' | 'name-asc' | 'name-desc' | 'size-asc' | 'size-desc';
type HeaderPanelMode = 'search' | 'download' | null;
type PrivacyTaskFilter = 'all' | 'private' | 'ordinary';

const backgroundPurgeFrameSize = 24;
const backgroundPurgeFrameDelayMs = 16;
const initialTaskSorts: Record<TaskListTab, TaskSortMode> = {
  all: 'default-all',
  active: 'created-desc',
  completed: 'finished-desc',
  failed: 'finished-desc',
};
const initialTaskOrders: Record<TaskListTab, string[]> = { all: [], active: [], completed: [], failed: [] };

interface TaskOrderPreferences {
  sorts: Record<TaskListTab, TaskSortMode>;
  orders: Record<TaskListTab, string[]>;
}

function taskSizeBytes(task: QueueTask): number | undefined {
  if (Number.isFinite(task.total_length)) return task.total_length;
  const selectedFiles = task.files?.filter((file) => file.selected !== false) ?? [];
  if (selectedFiles.length) return selectedFiles.reduce((total, file) => total + file.length, 0);
  const childSizes = task.children?.flatMap((child) => Number.isFinite(child.total_length) ? [child.total_length!] : []) ?? [];
  if (childSizes.length) return childSizes.reduce((total, size) => total + size, 0);
  return Number.isFinite(task.completed_length) ? task.completed_length : undefined;
}

function sortTasks(tasks: readonly QueueTask[], mode: TaskSortMode): QueueTask[] {
  if (mode === 'manual') return [...tasks];
  if (mode === 'default-all') return [
    ...sortTasks(tasks.filter((task) => isActiveDownloadTask(task.status)), 'created-desc'),
    ...sortTasks(tasks.filter((task) => !isActiveDownloadTask(task.status)), 'created-desc'),
  ];
  const [field, direction] = mode.split('-') as ['created' | 'finished' | 'progress' | 'name' | 'size', 'asc' | 'desc'];
  return tasks.map((task, index) => ({ task, index })).sort((left, right) => {
    let comparison = 0;
    if (field === 'name') {
      comparison = displayTaskName(left.task).localeCompare(displayTaskName(right.task), 'zh-CN', { numeric: true, sensitivity: 'base' });
    } else {
      const leftValue = field === 'created' ? left.task.created_at || left.task.updated_at || undefined
        : field === 'finished' ? left.task.status === 'failed'
          ? left.task.updated_at || left.task.created_at || undefined
          : left.task.completed_at || left.task.updated_at || left.task.created_at || undefined
        : field === 'progress' ? left.task.progress
          : taskSizeBytes(left.task);
      const rightValue = field === 'created' ? right.task.created_at || right.task.updated_at || undefined
        : field === 'finished' ? right.task.status === 'failed'
          ? right.task.updated_at || right.task.created_at || undefined
          : right.task.completed_at || right.task.updated_at || right.task.created_at || undefined
        : field === 'progress' ? right.task.progress
          : taskSizeBytes(right.task);
      if (leftValue === undefined || rightValue === undefined) {
        if (leftValue === undefined && rightValue !== undefined) return 1;
        if (leftValue !== undefined && rightValue === undefined) return -1;
      } else {
        comparison = leftValue - rightValue;
      }
    }
    if (comparison === 0) return left.index - right.index;
    return direction === 'asc' ? comparison : -comparison;
  }).map(({ task }) => task);
}

function prioritizeNewActiveTasks(tasks: readonly QueueTask[], newTaskIds: readonly string[]): QueueTask[] {
  if (!newTaskIds.length) return [...tasks];
  const byId = new Map(tasks.map((task) => [task.id, task]));
  const prioritizedIds = new Set(newTaskIds.filter((id) => {
    const task = byId.get(id);
    return task && isActiveDownloadTask(task.status);
  }));
  return [
    ...newTaskIds.flatMap((id) => {
      const task = byId.get(id);
      return task && prioritizedIds.has(id) ? [task] : [];
    }),
    ...tasks.filter((task) => !prioritizedIds.has(task.id)),
  ];
}

type TranslationKey = Parameters<typeof t>[0];
type TaskSortGroup = { labelKey: TranslationKey; options: ReadonlyArray<{ mode: TaskSortMode; labelKey: TranslationKey; ariaKey: TranslationKey }> };
const taskSortGroups: ReadonlyArray<TaskSortGroup> = [
  { labelKey: 'download.sort.createdAt', options: [
    { mode: 'created-desc', labelKey: 'download.sort.newest', ariaKey: 'download.sort.createdNewest' },
    { mode: 'created-asc', labelKey: 'download.sort.oldest', ariaKey: 'download.sort.createdOldest' },
  ] },
  { labelKey: 'download.sort.progress', options: [
    { mode: 'progress-asc', labelKey: 'download.sort.lowToHigh', ariaKey: 'download.sort.progressLowToHigh' },
    { mode: 'progress-desc', labelKey: 'download.sort.highToLow', ariaKey: 'download.sort.progressHighToLow' },
  ] },
  { labelKey: 'download.sort.fileName', options: [
    { mode: 'name-asc', labelKey: 'download.sort.aToZ', ariaKey: 'download.sort.fileNameAToZ' },
    { mode: 'name-desc', labelKey: 'download.sort.zToA', ariaKey: 'download.sort.fileNameZToA' },
  ] },
  { labelKey: 'download.sort.fileSize', options: [
    { mode: 'size-asc', labelKey: 'download.sort.smallToLarge', ariaKey: 'download.sort.fileSizeSmallToLarge' },
    { mode: 'size-desc', labelKey: 'download.sort.largeToSmall', ariaKey: 'download.sort.fileSizeLargeToSmall' },
  ] },
];
const privacyTaskFilterOptions: ReadonlyArray<{ value: Exclude<PrivacyTaskFilter, 'all'>; labelKey: TranslationKey }> = [
  { value: 'private', labelKey: 'download.filter.private' },
  { value: 'ordinary', labelKey: 'download.filter.ordinary' },
];

function PrivacyLockIcon({ unlocked }: { unlocked: boolean }) {
  return <svg aria-hidden="true" viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" strokeWidth="1.75" strokeLinecap="round" strokeLinejoin="round">
    <rect x="5" y="10" width="14" height="10" rx="2" />
    {unlocked ? <path d="M8 10V8a4 4 0 0 1 7.4-2.1" /> : <path d="M8 10V8a4 4 0 0 1 8 0v2" />}
    <path d="M12 14v2" />
  </svg>;
}

function TaskSortControl({ tab, value, onChange, privacyFilter, onPrivacyFilterChange, showPrivacyFilter }: { tab: TaskListTab; value: TaskSortMode; onChange(mode: TaskSortMode): void; privacyFilter: PrivacyTaskFilter; onPrivacyFilterChange(filter: PrivacyTaskFilter): void; showPrivacyFilter: boolean }) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const rootRef = useRef<HTMLDivElement>(null);
  const availableSortGroups: ReadonlyArray<TaskSortGroup> = tab === 'all'
    ? [{ labelKey: 'download.sort.default', options: [{ mode: 'default-all' as const, labelKey: 'download.sort.activeFirst', ariaKey: 'download.sort.defaultActiveFirst' }] }, ...taskSortGroups]
    : tab === 'completed' || tab === 'failed'
      ? [{ labelKey: 'download.sort.finishedAt', options: [
        { mode: 'finished-desc' as const, labelKey: 'download.sort.newest', ariaKey: 'download.sort.finishedNewest' },
        { mode: 'finished-asc' as const, labelKey: 'download.sort.oldest', ariaKey: 'download.sort.finishedOldest' },
      ] }, ...taskSortGroups]
      : taskSortGroups;
  useEffect(() => {
    if (!open) return;
    const closeFromOutside = (event: PointerEvent) => {
      if (!rootRef.current?.contains(event.target as Node)) setOpen(false);
    };
    const closeFromKeyboard = (event: KeyboardEvent) => {
      if (event.key === 'Escape') setOpen(false);
    };
    document.addEventListener('pointerdown', closeFromOutside);
    document.addEventListener('keydown', closeFromKeyboard);
    return () => {
      document.removeEventListener('pointerdown', closeFromOutside);
      document.removeEventListener('keydown', closeFromKeyboard);
    };
  }, [open]);
  return <div className="task-sort-control" ref={rootRef}>
    <button type="button" className="task-sort-trigger" aria-label={t('download.sort.tasks')} aria-haspopup="menu" aria-expanded={open} onClick={() => setOpen((current) => !current)}>
      <svg aria-hidden="true" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round"><path d="M10 6h9M10 12h6M10 18h3" /><path d="M5 5v14m-3-3 3 3 3-3" /></svg>
    </button>
    {open && <div className="task-sort-menu" role="menu" aria-label={t('download.sort.menu')}>
      {availableSortGroups.map((group) => <section key={group.labelKey}><p>{t(group.labelKey)}</p><div>{group.options.map((option) => <button type="button" key={option.mode} role="menuitemradio" aria-checked={value === option.mode} aria-label={t(option.ariaKey)} className={value === option.mode ? 'selected' : ''} onClick={() => { onChange(option.mode); setOpen(false); }}>{t(option.labelKey)}</button>)}</div></section>)}
      {showPrivacyFilter && <section><p>{t('download.filter.title')}</p><div>{privacyTaskFilterOptions.map((option) => {
        const checked = privacyFilter === 'all' || privacyFilter === option.value;
        const onlySelected = privacyFilter === option.value;
        return <button type="button" key={option.value} role="menuitemcheckbox" aria-checked={checked} aria-disabled={onlySelected} title={onlySelected ? t('download.filter.atLeastOne') : undefined} className={checked ? 'selected' : ''} onClick={() => {
          if (onlySelected) return;
          onPrivacyFilterChange(privacyFilter === 'all' ? (option.value === 'private' ? 'ordinary' : 'private') : 'all');
        }}>{t(option.labelKey)}</button>;
      })}</div></section>}
    </div>}
  </div>;
}

const hiddenTaskStatuses = new Set<QueueTask['status']>(['cancelled', 'recycled']);
const completedOrFailedStatuses = new Set<QueueTask['status']>(['completed', 'failed']);

function isActiveDownloadTask(status: QueueTask['status']) {
  return !hiddenTaskStatuses.has(status) && !completedOrFailedStatuses.has(status);
}

const cloudClient: CloudManagerClient = cloudApi;

function hasMultipleLines(value: string): boolean {
  return /[\r\n]/.test(value);
}

function droppedTorrentPaths(paths: readonly string[]): string[] {
  return paths.filter((path) => path.trim().toLocaleLowerCase().endsWith('.torrent'));
}

function dropPositionHitsElement(position: { toLogical(scaleFactor: number): { x: number; y: number } }, scaleFactor: number, element: HTMLElement | null): boolean {
  if (!element) return false;
  const point = position.toLogical(scaleFactor);
  const bounds = element.getBoundingClientRect();
  return point.x >= bounds.left && point.x <= bounds.right && point.y >= bounds.top && point.y <= bounds.bottom;
}

function isDownloadInput(value: string): boolean {
  return /(?:https?|ftp):\/\/|magnet:\?|ed2k:\/\/|thunder:\/\/|flashget:\/\/|cloud-share:\/\//i.test(value.trim());
}

function taskTabForSearchResult(task: QueueTask): TaskListTab {
  if (task.status === 'completed') return 'completed';
  if (task.status === 'failed') return 'failed';
  return 'active';
}

function taskStatusLabel(task: QueueTask): string {
  if (task.status === 'completed') return t('download.status.completed');
  if (task.status === 'failed') return t('download.status.failed');
  return t('download.status.active');
}

function searchTaskMatches(task: QueueTask, keyword: string): boolean {
  const normalizedKeyword = keyword.trim().toLocaleLowerCase();
  if (!normalizedKeyword || task.status === 'recycled' || task.status === 'cancelled') return false;
  return [displayTaskName(task), task.source, task.error].some((value) => value?.toLocaleLowerCase().includes(normalizedKeyword));
}

function isQueueTask(value: unknown): value is QueueTask {
  if (typeof value !== 'object' || value === null) return false;
  const task = value as Partial<QueueTask>;
  return typeof task.id === 'string' && typeof task.name === 'string' && typeof task.status === 'string' && typeof task.progress === 'number';
}

function findQueueTask(tasks: readonly QueueTask[], id: string): QueueTask | undefined {
  for (const task of tasks) {
    if (task.id === id) return task;
    const child = task.children && findQueueTask(task.children, id);
    if (child) return child;
  }
  return undefined;
}

function formatSpeed(bytes: number): string {
  const units = ['B/s', 'KB/s', 'MB/s', 'GB/s', 'TB/s'];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) { value /= 1024; unit += 1; }
  return unit === 0 ? `${Math.round(value)} ${units[unit]}` : `${value.toFixed(1)} ${units[unit]}`;
}

async function runWindowAction(action: 'minimize' | 'maximize' | 'hide') {
  if (!isTauriRuntime()) return;
  try {
    const { getCurrentWindow } = await import('@tauri-apps/api/window');
    const window = getCurrentWindow();
    if (action === 'minimize') await window.minimize();
    if (action === 'maximize') await window.toggleMaximize();
    if (action === 'hide') await window.hide();
  } catch {
    // Window chrome is inert in browser previews and during shutdown.
  }
}

async function browseDownloadDirectory(currentDirectory: string): Promise<string | null> {
  if (!isTauriRuntime()) return null;
  const { open } = await import('@tauri-apps/plugin-dialog');
  const selected = await open({ directory: true, multiple: false, defaultPath: currentDirectory || undefined, title: t('settings.directory.choose') });
  return typeof selected === 'string' ? selected : null;
}

function WindowIcon({ kind }: { kind: 'minimize' | 'maximize' | 'close' }) {
  return <svg aria-hidden="true" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" strokeLinejoin="round">
    {kind === 'minimize' && <path d="M5 12h14" />}
    {kind === 'maximize' && <rect x="5" y="5" width="14" height="14" rx="1" />}
    {kind === 'close' && <><path d="m6.5 6.5 11 11" /><path d="m17.5 6.5-11 11" /></>}
  </svg>;
}

function SidebarToggleIcon({ collapsed }: { collapsed: boolean }) {
  return <svg aria-hidden="true" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.75" strokeLinecap="round" strokeLinejoin="round">
    <rect x="4" y="4.5" width="16" height="15" rx="2" />
    <path d="M9 4.5v15" />
    {collapsed ? <path d="m12.5 9.5 3 2.5-3 2.5" /> : <path d="m15.5 9.5-3 2.5 3 2.5" />}
  </svg>;
}

function NavigationIcon({ kind }: { kind: 'tasks' | 'cloud' | 'settings' | 'about' }) {
  const svgProps = {
    'aria-hidden': true,
    viewBox: '0 0 24 24',
    fill: 'none',
    stroke: 'currentColor',
    strokeWidth: 2.15,
    strokeLinecap: 'round' as const,
    strokeLinejoin: 'round' as const,
  };

  if (kind === 'tasks') {
    return <svg {...svgProps}><path d="M12 3.5v10.25" /><path d="m8.25 10.5 3.75 3.75 3.75-3.75" /><path d="M5 16.5h14v3H5z" /></svg>;
  }
  if (kind === 'cloud') {
    return <svg {...svgProps}><path d="M7.1 18.5h9.55a3.35 3.35 0 0 0 .48-6.66A5.7 5.7 0 0 0 6.42 10.2 4.2 4.2 0 0 0 7.1 18.5Z" /><path d="M8.5 20h7" /></svg>;
  }
  if (kind === 'about') {
    return <svg {...svgProps}><circle cx="12" cy="12" r="8.5" /><path d="M12 11v5M12 7.5v.25" /></svg>;
  }
  return <svg {...svgProps}><path d="m12 3.1 1.62 1.7 2.3-.27.92 2.12 2.1.98-.52 2.24 1.48 1.62-1.48 1.62.52 2.24-2.1.98-.92 2.12-2.3-.27L12 20.9l-1.62-1.7-2.3.27-.92-2.12-2.1-.98.52-2.24-1.48-1.62 1.48-1.62-.52-2.24 2.1-.98.92-2.12 2.3.27L12 3.1Z" /><circle cx="12" cy="12" r="2.8" /></svg>;
}

export function App() {
  const { t } = useTranslation();
  const [tasks, setTasks] = useState<QueueTask[]>([]);
  const [newActiveTaskIds, setNewActiveTaskIds] = useState<string[]>([]);
  const [selectedTaskIds, setSelectedTaskIds] = useState<string[]>([]);
  const selectedTaskId = selectedTaskIds.at(-1) ?? null;
  const setSelectedTaskId = (next: string | null | ((current: string | null) => string | null)) => {
    setSelectedTaskIds((current) => {
      const currentId = current.at(-1) ?? null;
      const nextId = typeof next === 'function' ? next(currentId) : next;
      return nextId ? [nextId] : [];
    });
  };
  const [confirmation, setConfirmation] = useState<ParseResult | null>(null);
  const [duplicateQueue, setDuplicateQueue] = useState<DuplicateDownload[]>([]);
  const [resolvingDuplicateId, setResolvingDuplicateId] = useState<string | null>(null);
  const [view, setView] = useState<ManagerView | 'about'>('downloads');
  const [cloudPage, setCloudPage] = useState<CloudWorkspacePage>('management');
  const [authorizedCloudProviders, setAuthorizedCloudProviders] = useState<readonly ProviderDescriptor[]>([]);
  const [downloadTab, setDownloadTab] = useState<TaskListTab>('all');
  const [taskOrderPreferences, setTaskOrderPreferences] = useState<TaskOrderPreferences>(() => ({
    sorts: { ...initialTaskSorts }, orders: { ...initialTaskOrders },
  }));
  const taskSorts = taskOrderPreferences.sorts;
  const taskOrders = taskOrderPreferences.orders;
  const [searchInput, setSearchInput] = useState('');
  const [headerPanel, setHeaderPanel] = useState<HeaderPanelMode>(null);
  const [selectedSearchResultIndex, setSelectedSearchResultIndex] = useState<number | null>(null);
  const [headerConfirmation, setHeaderConfirmation] = useState<ParseResult | null>(null);
  const [settings, setSettings] = useState<AppSettings>(defaultSettings);
  const [appUpdate, setAppUpdate] = useState<AppUpdate | null>(null);
  const [privacyVisibility, setPrivacyVisibility] = useState<PrivacyTaskVisibility>({ enabled: false, unlocked: false });
  const [privacyTaskFilter, setPrivacyTaskFilter] = useState<PrivacyTaskFilter>('all');
  const [privacyUnlockOpen, setPrivacyUnlockOpen] = useState(false);
  const [privacyUnlockError, setPrivacyUnlockError] = useState<string | null>(null);
  const [privacyUnlocking, setPrivacyUnlocking] = useState(false);
  const [newDownloadOpen, setNewDownloadOpen] = useState(false);
  const [newDownloadInput, setNewDownloadInput] = useState('');
  const [detailsTaskId, setDetailsTaskId] = useState<string | null>(null);
  const [pendingPurgeTaskIds, setPendingPurgeTaskIds] = useState<string[] | null>(null);
  const [notice, setNotice] = useState<{ kind: 'success' | 'error'; message: string } | null>(null);
  const [taskReauthorization, setTaskReauthorization] = useState<CloudAuthorization | null>(null);
  const taskAuthorizationActive = useRef(false);
  const taskAuthorizationGeneration = useRef(0);
  useEffect(() => () => { taskAuthorizationActive.current = false; taskAuthorizationGeneration.current += 1; }, []);
  const [sidebarCollapsed, setSidebarCollapsed] = useState(false);
  const resolvedTheme = useSyncExternalStore(subscribeTheme, getResolvedTheme, getResolvedTheme);
  const resolvedThemeId = useSyncExternalStore(subscribeTheme, getResolvedThemeId, getResolvedThemeId);
  useEffect(() => {
    // Tauri's regular set_icon call only controls Windows' small title-bar
    // icon.  Keep the native taskbar icon in step with the concrete palette.
    void syncMainWindowIcon(resolvedThemeId).catch(() => undefined);
  }, [resolvedThemeId]);
  useEffect(() => {
    if (!isTauriRuntime()) return;
    const mainWindow = getCurrentWindow();
    let disposed = false;
    let unlisten: (() => void) | undefined;
    const synchronize = (nativeScaleFactor = 1) => {
      void applyMainWindowDpiSizing(mainWindow, nativeScaleFactor).catch(() => undefined);
    };

    void mainWindow.scaleFactor()
      .then((scaleFactor) => { if (!disposed) synchronize(scaleFactor); })
      .catch(() => { if (!disposed) synchronize(); });
    mainWindow.onScaleChanged(({ payload }) => {
      if (!disposed) synchronize(payload.scaleFactor);
    }).then((stop) => { if (disposed) stop(); else unlisten = stop; }).catch(() => undefined);

    return () => {
      disposed = true;
      unlisten?.();
    };
  }, []);
  const privacyVisibleTasks = useMemo(() => tasks.filter((task) => privacyVisibility.unlocked || !isPrivateTask(task)), [privacyVisibility.unlocked, tasks]);
  const filteredPrivacyTasks = useMemo(() => privacyVisibleTasks.filter((task) => {
    if (privacyTaskFilter === 'all') return true;
    return privacyTaskFilter === 'private' ? isPrivateTask(task) : !isPrivateTask(task);
  }), [privacyTaskFilter, privacyVisibleTasks]);
  const allTasks = useMemo(() => filteredPrivacyTasks.filter((task) => !hiddenTaskStatuses.has(task.status)), [filteredPrivacyTasks]);
  const activeTasks = useMemo(() => allTasks.filter((task) => isActiveDownloadTask(task.status)), [allTasks]);
  const completedTasks = useMemo(() => allTasks.filter((task) => task.status === 'completed'), [allTasks]);
  const failedTasks = useMemo(() => allTasks.filter((task) => task.status === 'failed'), [allTasks]);
  const taskSearchResults = useMemo(() => headerPanel === 'search' && !isDownloadInput(searchInput)
    ? filteredPrivacyTasks.filter((task) => searchTaskMatches(task, searchInput))
    : [], [filteredPrivacyTasks, headerPanel, searchInput]);
  const unsortedVisibleTasks = downloadTab === 'all' ? allTasks
    : downloadTab === 'active' ? activeTasks
      : downloadTab === 'completed' ? completedTasks
        : failedTasks;
  const visibleTasks = useMemo(() => {
    const sorted = taskSorts[downloadTab] === 'manual'
      ? applyStoredTaskOrder(unsortedVisibleTasks, taskOrders[downloadTab])
      : sortTasks(unsortedVisibleTasks, taskSorts[downloadTab]);
    const mode = taskSorts[downloadTab];
    return mode === 'default-all' || mode === 'created-desc' ? prioritizeNewActiveTasks(sorted, newActiveTaskIds) : sorted;
  }, [downloadTab, newActiveTaskIds, taskOrders, taskSorts, unsortedVisibleTasks]);
  const detailsTask = useMemo(() => filteredPrivacyTasks.flatMap((task) => [task, ...(task.children ?? [])]).find((task) => task.id === detailsTaskId), [detailsTaskId, filteredPrivacyTasks]);
  const downloadingTasks = filteredPrivacyTasks.filter((task) => task.status === 'downloading');
  const currentSpeed = formatSpeed(downloadingTasks.reduce((total, task) => total + (task.download_speed ?? 0), 0));
  const pendingPurgeTasks = (pendingPurgeTaskIds ?? []).flatMap((id) => {
    const task = privacyVisibleTasks.find((item) => item.id === id);
    return task ? [task] : [];
  });
  const [eventsReady, setEventsReady] = useState(false);
  const [engineState, setEngineState] = useState<DownloadEngineState>({ available: true, retrying: false });
  const taskUpdates = useRef(createTaskUpdateMerger());
  const purgingTaskIds = useRef(new Set<string>());
  const permanentlyPurgedTaskIds = useRef(new Set<string>());
  const purgeInProgressRef = useRef(false);
  const engineEventEpoch = useRef(0);
  const privacyUnlockPassword = useRef<HTMLInputElement>(null);
  const previousPrivacyUnlocked = useRef(false);
  const tasksRef = useRef<QueueTask[]>(tasks);
  const settingsRef = useRef<AppSettings>(settings);
  const terminalNotificationStatuses = useRef(new Map<string, QueueTask['status']>());
  // Keep a locally requested start visible until its RPC action settles.  A
  // polling snapshot can otherwise put the old paused/failed state back on
  // screen while aria2 is still handling the request.
  const pendingStartTaskIds = useRef(new Set<string>());
  const searchDropTarget = useRef<HTMLLabelElement>(null);
  const confirmationGeneration = useRef(0);
  const headerConfirmationGeneration = useRef(0);
  const shareConfirmationsInFlight = useRef(new Set<string>());
  const duplicateActionInFlight = useRef(false);
  const duplicateCloseInFlight = useRef(false);
  tasksRef.current = tasks;
  settingsRef.current = settings;
  const duplicate = resolvingDuplicateId ? null : duplicateQueue[0] ?? null;

  const windowInteraction = useRef<ReturnType<typeof createWindowInteraction> | null>(null);
  const [nativeWindowDrag, setNativeWindowDrag] = useState(false);
  useEffect(() => {
    const interaction = createWindowInteraction();
    windowInteraction.current = interaction;
    const stopResizeMode = installWindowResizePerformanceMode(interaction.subscribe);
    const stopNative = isTauriRuntime() ? connectWindowInteraction(
      interaction,
      (handler) => listen<boolean>('main-window-interaction', ({ payload }) => handler(payload)),
      () => invoke<WindowInteractionSnapshot>('main_window_interaction_state'),
      (enabled) => setNativeWindowDrag(supportsNativeWindowDrag(enabled)),
    ) : undefined;
    return () => {
      stopNative?.();
      stopResizeMode();
      interaction.dispose();
      if (windowInteraction.current === interaction) windowInteraction.current = null;
    };
  }, []);

  useEffect(() => {
    if (!isTauriRuntime()) return;
    const currentWindow = getCurrentWindow();
    let disposed = false;
    let unlisten: (() => void) | undefined;
    let scaleFactor = window.devicePixelRatio || 1;
    void currentWindow.scaleFactor().then((value) => { if (!disposed && Number.isFinite(value) && value > 0) scaleFactor = value; }).catch(() => undefined);
    currentWindow.onDragDropEvent((event) => {
      if (disposed || event.payload.type !== 'drop') return;
      const paths = droppedTorrentPaths(event.payload.paths);
      if (!paths.length || !dropPositionHitsElement(event.payload.position, scaleFactor, searchDropTarget.current)) return;
      setView('downloads');
      setDownloadTab('active');
      setHeaderPanel(null);
      setSearchInput('');
      setNewDownloadInput(paths.join('\n'));
      setNewDownloadOpen(true);
    }).then((stop) => { if (disposed) stop(); else unlisten = stop; }).catch(() => undefined);
    return () => { disposed = true; unlisten?.(); };
  }, []);

  function enqueueDuplicate(next: DuplicateDownload) {
    setDuplicateQueue((current) => [...current, next]);
  }

  function showConfirmation(next: ParseResult) {
    confirmationGeneration.current += 1;
    setConfirmation(next);
  }

  function dismissConfirmation() {
    const prepared = confirmation;
    confirmationGeneration.current += 1;
    setConfirmation(null);
    if (prepared && !prepared.taskId.startsWith('cloud-share://')) void discardPreparedDownload(prepared.taskId).catch(() => undefined);
  }

  function showHeaderConfirmation(next: ParseResult) {
    headerConfirmationGeneration.current += 1;
    setHeaderConfirmation(next);
  }

  function dismissHeaderConfirmation() {
    const prepared = headerConfirmation;
    headerConfirmationGeneration.current += 1;
    setHeaderConfirmation(null);
    if (prepared && !prepared.taskId.startsWith('cloud-share://')) void discardPreparedDownload(prepared.taskId).catch(() => undefined);
  }

  function dismissDuplicate(taskId: string) {
    if (duplicateActionInFlight.current || duplicateCloseInFlight.current) return;
    duplicateCloseInFlight.current = true;
    setDuplicateQueue((current) => current[0]?.task.id === taskId ? current.slice(1) : current);
    window.setTimeout(() => { duplicateCloseInFlight.current = false; }, 0);
  }

  function applyTaskUpdates(
    incoming: readonly QueueTaskInput[],
    request?: { version: number; scope?: TaskSnapshotScope; protectPurgeInProgress?: boolean },
  ) {
    const normalized = normalizeQueueTasks(incoming);
    const protectedIds = new Set(permanentlyPurgedTaskIds.current);
    if (request?.protectPurgeInProgress !== false) {
      for (const id of purgingTaskIds.current) protectedIds.add(id);
    }
    if (request) {
      for (const id of pendingStartTaskIds.current) protectedIds.add(id);
    }
    const next = taskUpdates.current.merge(tasksRef.current, normalized, {
      requestVersion: request?.version, scope: request?.scope, protectedIds,
    });
    tasksRef.current = next;
    setTasks(next);
    return next;
  }

  function applyTaskProjection(task: QueueTaskInput) {
    const normalized = normalizeQueueTask(task);
    applyTaskUpdates([normalized]);
    return normalized;
  }

  function addNewDownloadTask(task: QueueTask) {
    applyTaskProjection(task);
    if (isActiveDownloadTask(task.status)) {
      // Only default/newest-first views prioritize new tasks. An explicit
      // name, size, progress or manual order remains authoritative.
      setNewActiveTaskIds((current) => [task.id, ...current.filter((id) => id !== task.id)]);
    }
  }

  function queueTaskOptimistically(task: QueueTask) {
    pendingStartTaskIds.current.add(task.id);
    applyTaskProjection({
      ...task,
      status: 'queued',
      speed: t('download.waitingToStart'),
      download_speed: 0,
      upload_speed: 0,
      eta: undefined,
      eta_seconds: undefined,
      statusMessage: t('download.submitting'),
      error: undefined,
      failure_code: undefined,
      reauthorization_required: undefined,
    });
  }

  function restoreTaskAfterStartFailure(task: QueueTask, error: unknown) {
    const message = error && typeof error === 'object' && 'message' in error && typeof error.message === 'string'
      ? error.message : String(error);
    applyTaskProjection({ ...task, statusMessage: undefined, error: message });
    return message;
  }

  function protectRemovedTasksFromOlderSnapshots(taskIds: readonly string[]) {
    taskUpdates.current.protectRemoved(taskIds);
  }

  function refreshTaskSnapshot(
    disposed: () => boolean,
    scope?: 'active' | 'terminal',
    protectPurgeInProgress = true,
  ) {
    const requestVersion = taskUpdates.current.beginSnapshot();
    const snapshot = scope ? listPersistedTasks(scope) : listPersistedTasks();
    return snapshot.then((saved) => {
      if (disposed()) return [] as QueueTask[];
      applyTaskUpdates(saved, { version: requestVersion, scope, protectPurgeInProgress });
      return normalizeQueueTasks(saved);
    }).catch((error: unknown) => {
      if (!disposed()) setNotice({ kind: 'error', message: String(error) });
      return [] as QueueTask[];
    });
  }

  useEffect(() => {
    if (!notice) return;
    const timer = window.setTimeout(() => setNotice(null), 2600);
    return () => window.clearTimeout(timer);
  }, [notice]);

  useEffect(() => {
    if (!taskReauthorization) return;
    let cancelled = false;
    let completed = false;
    let unlisten: (() => void) | undefined;
    let timer: number | undefined;
    const generation = taskAuthorizationGeneration.current;
    const current = () => !cancelled && generation === taskAuthorizationGeneration.current;
    const finish = async () => {
      if (!current() || completed) return;
      completed = true;
      if (timer !== undefined) window.clearTimeout(timer);
      await refreshTaskSnapshot(() => cancelled, 'active');
      await refreshTaskSnapshot(() => cancelled, 'terminal');
      if (current()) {
        taskAuthorizationActive.current = false;
        setTaskReauthorization(null);
        setNotice({ kind: 'success', message: t('cloud.authorization.restoringTasks') });
      }
    };
    let querying = false;
    const wake = async () => {
      if (!current() || completed || querying) return;
      querying = true;
      try {
        const account = await cloudApi.getCompletedCloudAuthorization(taskReauthorization.provider);
        if (current() && account?.provider === taskReauthorization.provider) await finish();
      } catch { /* Watchdog covers missed completion events. */ }
      finally { querying = false; }
    };
    void cloudApi.subscribeCloudAuthorization(taskReauthorization.provider, event => {
      if (event.state === 'Ready') void wake();
    }).then(stop => { if (cancelled) stop(); else unlisten = stop; }).catch(() => {});
    const poll = () => {
      timer = window.setTimeout(async () => {
        try {
          await cloudApi.completeCloudAuthorization(taskReauthorization.provider);
          await finish();
        } catch (error) {
          const code = typeof error === 'object' && error !== null && 'code' in error
            ? String((error as { code?: unknown }).code)
            : '';
          if (current() && !completed && code !== 'authorization_pending') {
            taskAuthorizationActive.current = false;
            setTaskReauthorization(null);
            setNotice({ kind: 'error', message: error instanceof Error ? error.message : String(error) });
            return;
          }
        }
        if (current() && !completed) poll();
      }, 10_000);
    };
    poll();
    return () => {
      cancelled = true;
      unlisten?.();
      if (timer !== undefined) window.clearTimeout(timer);
    };
  }, [taskReauthorization]);

  useEffect(() => {
    if (!isTauriRuntime()) return;
    let disposed = false;
    let unlisten: (() => void) | undefined;
    let terminalLoadTimer: number | undefined;
    subscribeToDownloadEvents({
      onTask: (incoming) => {
        const task = normalizeQueueTask(incoming);
        if (disposed || purgingTaskIds.current.has(task.id) || permanentlyPurgedTaskIds.current.has(task.id)) return;
        const previousStatus = terminalNotificationStatuses.current.get(task.id);
        if (task.status === 'completed' || task.status === 'failed') {
          if (settingsRef.current.downloadStatusNotifications !== false && shouldNotifyTask(task, previousStatus)) {
            void sendDownloadNotification(task);
          }
          terminalNotificationStatuses.current.set(task.id, task.status);
        } else {
          terminalNotificationStatuses.current.delete(task.id);
        }
        applyTaskUpdates([task]);
        setConfirmation((current) => {
          if (!current || current.taskId !== task.id || !task.files?.length) return current;
          return {
            ...current,
            resources: task.files.map((file) => ({
              id: String(file.index), index: file.index, name: file.path,
              size: `${file.completed_length} B / ${file.length} B`,
              selected: current.resources.find((resource) => resource.index === file.index || resource.id === String(file.index))?.selected ?? file.selected
            }))
          };
        });
        setHeaderConfirmation((current) => {
          if (!current || current.taskId !== task.id || !task.files?.length) return current;
          return {
            ...current,
            resources: task.files.map((file) => ({
              id: String(file.index), index: file.index, name: file.path,
              size: `${file.completed_length} B / ${file.length} B`,
              selected: current.resources.find((resource) => resource.index === file.index || resource.id === String(file.index))?.selected ?? file.selected
            }))
          };
        });
      },
      onGlobalStat: () => undefined
    }).then((stop) => {
      if (disposed) stop();
      else {
        unlisten = stop;
        setEventsReady(true);
        void refreshTaskSnapshot(() => disposed, 'active').then(() => {
          if (disposed) return;
          terminalLoadTimer = window.setTimeout(() => {
            void refreshTaskSnapshot(() => disposed, 'terminal');
          }, 0);
        });
      }
    }).catch((error: unknown) => {
      if (!disposed) setNotice({ kind: 'error', message: String(error) });
    });
    return () => {
      disposed = true;
      if (terminalLoadTimer !== undefined) window.clearTimeout(terminalLoadTimer);
      unlisten?.();
    };
  }, []);

  useEffect(() => {
    const activeConfirmation = headerConfirmation ?? confirmation;
    const setActiveConfirmation = headerConfirmation ? setHeaderConfirmation : setConfirmation;
    const pendingMagnet = activeConfirmation?.isBitTorrent && activeConfirmation.resources.length === 0;
    if (!pendingMagnet || !isTauriRuntime()) return;
    let disposed = false;
    const refreshMetadata = () => {
      const requestVersion = taskUpdates.current.beginSnapshot();
      void listPersistedTasks().then((saved) => {
        if (disposed) return;
        const snapshot = normalizeQueueTasks(saved).find((item) => item.id === activeConfirmation.taskId);
        if (!snapshot) return;
        const accepted = applyTaskUpdates([snapshot], { version: requestVersion });
        const task = accepted.find((item) => item.id === activeConfirmation.taskId);
        if (!task) return;
        const files = task.files;
        if (!files?.length) return;
        setActiveConfirmation((current) => {
          if (!current || current.taskId !== task.id) return current;
          return {
            ...current,
            resources: files.map((file) => ({
              id: String(file.index), index: file.index, name: file.path,
              size: `${file.completed_length} B / ${file.length} B`,
              selected: current.resources.find((resource) => resource.index === file.index || resource.id === String(file.index))?.selected ?? file.selected
            }))
          };
        });
      }).catch(() => undefined);
    };
    refreshMetadata();
    const timer = window.setInterval(refreshMetadata, 500);
    return () => { disposed = true; window.clearInterval(timer); };
  }, [confirmation?.isBitTorrent, confirmation?.resources.length, confirmation?.taskId, headerConfirmation?.isBitTorrent, headerConfirmation?.resources.length, headerConfirmation?.taskId]);

  useEffect(() => {
    if (!isTauriRuntime() || !eventsReady) return;
    let disposed = false;
    let stopFocus: (() => void) | undefined;
    let terminalRefreshTimer: number | undefined;
    const refreshTasks = () => {
      void refreshTaskSnapshot(() => disposed, 'active').then(() => {
        if (disposed) return;
        if (terminalRefreshTimer !== undefined) window.clearTimeout(terminalRefreshTimer);
        terminalRefreshTimer = window.setTimeout(() => {
          windowInteraction.current?.schedule('focus-terminal', () => {
            if (!disposed) void refreshTaskSnapshot(() => disposed, 'terminal');
          });
        }, 0);
      });
    };
    getCurrentWindow().onFocusChanged(({ payload }) => {
      if (payload) windowInteraction.current?.schedule('focus', () => { if (!disposed) refreshTasks(); });
    })
      .then((stop) => { if (disposed) stop(); else stopFocus = stop; })
      .catch(() => undefined);
    return () => {
      disposed = true;
      if (terminalRefreshTimer !== undefined) window.clearTimeout(terminalRefreshTimer);
      stopFocus?.();
    };
  }, [eventsReady]);

  useEffect(() => {
    if (!isTauriRuntime() || !eventsReady || activeTasks.length === 0) return;
    let disposed = false;
    let refreshing = false;
    const refreshActiveProgress = async () => {
      if (refreshing) return;
      refreshing = true;
      try {
        const previousTasks = tasksRef.current;
        const activeSnapshot = await refreshTaskSnapshot(() => disposed, 'active');
        if (disposed) return;
        const activeIds = new Set(activeSnapshot.map((task) => task.id));
        const hasMissingActiveTask = previousTasks.some((task) => (
          isActiveDownloadTask(task.status) && !activeIds.has(task.id)
        ));
        if (hasMissingActiveTask) await refreshTaskSnapshot(() => disposed, 'terminal');
      } finally {
        refreshing = false;
      }
    };
    const timer = window.setInterval(() => { void refreshActiveProgress(); }, 1_000);
    return () => {
      disposed = true;
      window.clearInterval(timer);
    };
  }, [activeTasks.length, eventsReady]);

  useEffect(() => {
    if (!isTauriRuntime()) return;
    let disposed = false;
    let unlisten: (() => void) | undefined;
    listen<DownloadEngineState>('download-engine-state-updated', (event) => {
      engineEventEpoch.current += 1;
      if (!disposed) setEngineState(event.payload);
    }).then((stop) => {
      if (disposed) { stop(); return; }
      unlisten = stop;
      const requestEpoch = engineEventEpoch.current;
      void getDownloadEngineState().then((state) => {
        if (!disposed && engineEventEpoch.current === requestEpoch) setEngineState(state);
      }).catch((error: unknown) => {
        if (!disposed && engineEventEpoch.current === requestEpoch) setEngineState({ available: false, retrying: false, error: String(error) });
      });
    }).catch((error: unknown) => {
      if (!disposed) setEngineState({ available: false, retrying: false, error: String(error) });
    });
    return () => { disposed = true; unlisten?.(); };
  }, []);

  useEffect(() => {
    let active = true;
    getSettings().then((loaded) => { if (active) setSettings(loaded); }).catch(() => undefined);
    getPrivacyTaskVisibility().then((visibility) => { if (active) setPrivacyVisibility(visibility); }).catch(() => undefined);
    return () => { active = false; };
  }, []);

  useEffect(() => {
    let active = true;
    checkForAppUpdate().then((update) => { if (active) setAppUpdate(update); }).catch(() => undefined);
    return () => { active = false; };
  }, []);

  useEffect(() => {
    const wasUnlocked = previousPrivacyUnlocked.current;
    previousPrivacyUnlocked.current = privacyVisibility.unlocked;
    if (privacyVisibility.unlocked) return;
    setPrivacyTaskFilter('all');
    const visibleTaskIds = new Set(privacyVisibleTasks.map((task) => task.id));
    setSelectedTaskIds((ids) => {
      const visibleSelectedIds = ids.filter((id) => visibleTaskIds.has(id));
      return visibleSelectedIds.length === ids.length ? ids : visibleSelectedIds;
    });
    setDetailsTaskId((id) => id && privacyVisibleTasks.some((task) => task.id === id) ? id : null);
    if (!wasUnlocked) return;
    setConfirmation(null);
    setHeaderConfirmation(null);
    setDuplicateQueue([]);
    setPendingPurgeTaskIds(null);
    setTaskReauthorization(null);
  }, [privacyVisibility.unlocked, privacyVisibleTasks]);

  useEffect(() => {
    if (privacyUnlockOpen) privacyUnlockPassword.current?.focus();
  }, [privacyUnlockOpen]);

  function saveSettings(next: AppSettings): Promise<void> {
    const previous = settings;
    setSettings(next);
    return updateSettings(next).then(
      (saved) => { setSettings(saved); },
      (error) => {
        setSettings(previous);
        throw error;
      }
    );
  }

  async function saveAiConfiguration(configuration: AiSettingsInput) {
    const saved = await updateAiSettings(configuration);
    setSettings((current) => ({ ...current, ai: saved }));
    return saved;
  }

  async function savePrivacyConfiguration(enabled: boolean, currentPassword?: string, newPassword?: string) {
    const saved = await configurePrivacyDownload(enabled, currentPassword, newPassword);
    setSettings(saved);
    setPrivacyVisibility(await getPrivacyTaskVisibility());
    return saved;
  }

  async function togglePrivacyVisibility() {
    if (privacyVisibility.unlocked) {
      setPrivacyVisibility(await lockPrivacyTasks());
      return;
    }
    setPrivacyUnlockError(null);
    setPrivacyUnlockOpen(true);
  }

  async function unlockPrivateTasks() {
    const input = privacyUnlockPassword.current;
    const password = input?.value ?? '';
    if (input) input.value = '';
    if (!password) {
      setPrivacyUnlockError(t('privacy.passwordRequired'));
      return;
    }
    setPrivacyUnlocking(true);
    try {
      const visibility = await unlockPrivacyTasks(password);
      setPrivacyVisibility(visibility);
      setPrivacyUnlockOpen(false);
      if (visibility.unlocked) {
        await refreshTaskSnapshot(() => false, 'active');
        await refreshTaskSnapshot(() => false, 'terminal');
      }
    } catch (error) {
      setPrivacyUnlockError(error instanceof Error ? error.message : String(error));
    } finally {
      setPrivacyUnlocking(false);
    }
  }

  async function runLauncherBatchAction(command: 'pause' | 'resume') {
    const targets = tasksRef.current.filter((task) => (privacyVisibility.unlocked || !isPrivateTask(task)) && (command === 'pause' ? ['downloading', 'queued'].includes(task.status) : task.status === 'paused'));
    if (!targets.length) return;
    const actionTargets = command === 'resume'
      ? targets.filter((task) => !pendingStartTaskIds.current.has(task.id))
      : targets;
    if (!actionTargets.length) return;
    const optimisticTargets = command === 'resume' ? actionTargets : [];
    optimisticTargets.forEach(queueTaskOptimistically);
    const results = await settleTaskOperations(actionTargets.map((task) => ({ task, run: () => runTaskAction(task.id, command) })), command === 'pause');
    let failure: unknown;
    let refreshNeeded = false;
    for (const [index, result] of results.entries()) {
      const task = actionTargets[index];
      if (result.status === 'rejected') {
        failure ??= result.reason;
        if (command === 'resume' && optimisticTargets.some((target) => target.id === task.id)) {
          restoreTaskAfterStartFailure(task, result.reason);
        }
        pendingStartTaskIds.current.delete(task.id);
        continue;
      }
      const snapshot = result.value.queueTask ?? result.value.task;
      if (snapshot) {
        applyTaskProjection(snapshot);
      } else if (command === 'resume') {
        refreshNeeded = true;
      }
      pendingStartTaskIds.current.delete(task.id);
    }
    if (failure) setNotice({ kind: 'error', message: failure instanceof Error ? failure.message : String(failure) });
    if (refreshNeeded) void refreshTaskSnapshot(() => false, 'active');
  }

  useEffect(() => {
    if (!isTauriRuntime()) return;
    let disposed = false;
    let unlisten: (() => void) | undefined;
    listen<unknown>('launcher-navigate', (event) => {
      const destination = managerViewFromLauncherPayload(event.payload);
      if (destination) {
        setView(destination);
        if (destination !== 'downloads') setSidebarCollapsed(false);
      }
    }).then((stop) => { if (disposed) stop(); else unlisten = stop; }).catch(() => undefined);
    return () => { disposed = true; unlisten?.(); };
  }, []);

  useEffect(() => {
    if (!isTauriRuntime()) return;
    let disposed = false;
    let unlisten: (() => void) | undefined;
    listen<string>('launcher-new-download', (event) => {
      const input = event.payload.trim();
      if (!input) return;
      setView('downloads');
      setDownloadTab('active');
      setHeaderPanel(null);
      setNewDownloadInput(input);
      setNewDownloadOpen(true);
    }).then((stop) => { if (disposed) stop(); else unlisten = stop; }).catch(() => undefined);
    return () => { disposed = true; unlisten?.(); };
  }, []);

  useEffect(() => {
    if (!isTauriRuntime()) return;
    let disposed = false;
    let unlisten: (() => void) | undefined;
    // Links clicked in a browser wait in the native app until this window takes
    // them: the one that launched the app, and any that arrive while it runs.
    // Taking them drains the queue, so they are shown even if this effect was
    // torn down meanwhile (a StrictMode remount) rather than lost.
    const openExternalLinks = () => {
      void takeExternalLinks().then((links) => {
        if (!links.length) return;
        setView('downloads');
        setDownloadTab('active');
        setHeaderPanel(null);
        setNewDownloadInput(links.join('\n'));
        setNewDownloadOpen(true);
      }).catch(() => undefined);
    };
    listen('external-links-available', openExternalLinks)
      .then((stop) => { if (disposed) stop(); else unlisten = stop; })
      .catch(() => undefined);
    openExternalLinks();
    return () => { disposed = true; unlisten?.(); };
  }, []);

  useEffect(() => {
    if (!isTauriRuntime()) return;
    let disposed = false;
    let unlisten: (() => void) | undefined;
    listen<'new-download' | 'resume-all' | 'pause-all'>('launcher-command', async (event) => {
      if (event.payload === 'new-download') {
        let input = '';
        try {
          const { readText } = await import('@tauri-apps/plugin-clipboard-manager');
          const clipboard = await readText();
          if (isDownloadInput(clipboard)) input = clipboard.trim();
        } catch {
          // Clipboard access is optional; manual entry remains available.
        }
        setView('downloads');
        setDownloadTab('active');
        setNewDownloadInput(input);
        setNewDownloadOpen(true);
      }
      if (event.payload === 'resume-all') void runLauncherBatchAction('resume');
      if (event.payload === 'pause-all') void runLauncherBatchAction('pause');
    }).then((stop) => { if (disposed) stop(); else unlisten = stop; }).catch(() => undefined);
    return () => { disposed = true; unlisten?.(); };
  }, []);

  useEffect(() => {
    if (!isTauriRuntime()) return;
    let disposed = false;
    const unlisten: (() => void)[] = [];
    // This generic native input entry point is retained for non-launcher
    // sources. The floating launcher itself no longer emits it on left click.
    listen<string>('launcher-parse', (event) => {
      void parse(event.payload).catch((error) => setNotice({ kind: 'error', message: error instanceof Error ? error.message : String(error) }));
    }).then((stop) => { if (disposed) stop(); else unlisten.push(stop); }).catch(() => undefined);
    listen('launcher-open-all-tasks', () => {
      setView('downloads');
      setDownloadTab('all');
      setTaskOrderPreferences((current) => ({
        sorts: { ...current.sorts, all: initialTaskSorts.all },
        orders: { ...current.orders, all: [] },
      }));
      setHeaderPanel(null);
    }).then((stop) => { if (disposed) stop(); else unlisten.push(stop); }).catch(() => undefined);
    return () => { disposed = true; unlisten.forEach((stop) => stop()); };
  }, []);

  async function parse(input: string, accessCode?: string, confirmationPlacement: 'modal' | 'header' = 'modal') {
    const result = await parseDownload(input, accessCode, settings.privacyDownloadEnabled ?? false);
    if (result.task) { addNewDownloadTask(result.task); setSelectedTaskId(result.task.id); }
    if (result.confirmation) {
      if (confirmationPlacement === 'header') showHeaderConfirmation(result.confirmation);
      else showConfirmation(result.confirmation);
    }
    if (result.duplicate) {
      applyTaskProjection(result.duplicate.task);
      setSelectedTaskId(result.duplicate.task.id);
      enqueueDuplicate(result.duplicate);
    }
    return result;
  }

  async function resolveDuplicate(action: 'resume' | 'restart_missing' | 'overwrite') {
    if (!duplicate || duplicateActionInFlight.current) return;
    duplicateActionInFlight.current = true;
    const taskId = duplicate.task.id;
    setResolvingDuplicateId(taskId);
    try {
      const updated = await resolveDuplicateDownload(taskId, action);
      if (updated) {
        applyTaskProjection(updated);
        setSelectedTaskId(updated.id);
      }
      setDuplicateQueue((current) => current[0]?.task.id === taskId ? current.slice(1) : current);
    } catch (error) {
      setNotice({ kind: 'error', message: error instanceof Error ? error.message : String(error) });
    } finally {
      duplicateActionInFlight.current = false;
      setResolvingDuplicateId((current) => current === taskId ? null : current);
    }
  }

  async function openDuplicateDirectory() {
    if (!duplicate || duplicateActionInFlight.current) return;
    duplicateActionInFlight.current = true;
    const taskId = duplicate.task.id;
    try {
      await openTaskDirectory(taskId);
      setDuplicateQueue((current) => current[0]?.task.id === taskId ? current.slice(1) : current);
    } catch (error) {
      setNotice({ kind: 'error', message: error instanceof Error ? error.message : String(error) });
    } finally {
      duplicateActionInFlight.current = false;
    }
  }

  async function confirm(selectedIds: string[], destinationDirectory?: string, privateDownload = false, placement: 'modal' | 'header' = 'modal') {
    const activeConfirmation = placement === 'header' ? headerConfirmation : confirmation;
    if (!activeConfirmation) return;
    const activeGeneration = placement === 'header' ? headerConfirmationGeneration : confirmationGeneration;
    const setActiveConfirmation = placement === 'header' ? setHeaderConfirmation : setConfirmation;
    const planKey = `${placement}:${activeGeneration.current}`;
    if (activeConfirmation.taskId.startsWith('cloud-share://')) {
      const planSource = activeConfirmation.taskId;
      const requestGeneration = activeGeneration.current;
      if (shareConfirmationsInFlight.current.has(planKey)) return;
      shareConfirmationsInFlight.current.add(planKey);
      let createdTaskId: string | undefined;
      const onCreated = (task: QueueTask) => {
        addNewDownloadTask(task);
        if (createdTaskId || activeGeneration.current !== requestGeneration) return;
        createdTaskId = task.id;
        setActiveConfirmation(null);
        setHeaderPanel(null);
        setSearchInput('');
        setView('downloads');
        setDownloadTab('active');
        setSelectedTaskId(task.id);
        window.setTimeout(() => {
          Array.from(document.querySelectorAll<HTMLElement>('[data-task-id]'))
            .find((element) => element.dataset.taskId === task.id)
            ?.scrollIntoView?.({ block: 'center', behavior: 'smooth' });
        }, 0);
      };
      try {
        const result = await confirmCloudShareDownload(planSource, selectedIds, destinationDirectory, privateDownload, onCreated);
        const outcomes = result.outcomes;
        const cloudTasks = outcomes.flatMap((outcome) => outcome.queueTask ? [outcome.queueTask] : outcome.task ? [outcome.task] : []);
        const duplicates = outcomes.flatMap((outcome) => outcome.duplicate ? [outcome.duplicate] : []);
        cloudTasks.forEach(addNewDownloadTask);
        duplicates.forEach(({ task }) => applyTaskProjection(task));
        if (!createdTaskId && result.failures.length === 0 && cloudTasks.length) onCreated(cloudTasks[0]);
        setDuplicateQueue((current) => [...current, ...duplicates]);
        const failedTask = cloudTasks.find((task) => task.status === 'failed');
        if (failedTask && activeGeneration.current === requestGeneration) {
          setNotice({ kind: 'error', message: t('download.submitFailedRetry', { name: failedTask.name, error: failedTask.error || t('download.submitFailed'), tab: t('download.tabs.active') }) });
        }
        if (result.failures.length > 0) {
          const failedIds = new Set(result.failures.map((failure) => failure.selectedId));
          setActiveConfirmation((current) => activeGeneration.current === requestGeneration && (current?.taskId === planSource || (!current && createdTaskId)) ? {
            ...activeConfirmation,
            resources: activeConfirmation.resources.map((resource) => ({ ...resource, selected: failedIds.has(resource.id) })),
          } : current);
          if (activeGeneration.current === requestGeneration) {
            const first = result.failures[0];
            const prefix = result.failures.length > 1 ? t('download.filesFailedPrefix', { count: result.failures.length }) : '';
            setNotice({ kind: 'error', message: `${prefix}${first.message}` });
          }
        } else {
          setActiveConfirmation((current) => current?.taskId === planSource && activeGeneration.current === requestGeneration ? null : current);
        }
      } catch (error) {
        if (activeGeneration.current === requestGeneration) {
          if (createdTaskId) setActiveConfirmation((current) => current ?? activeConfirmation);
          setNotice({ kind: 'error', message: error instanceof Error ? error.message : String(error) });
        }
      } finally {
        shareConfirmationsInFlight.current.delete(planKey);
      }
      return;
    }
    const task: QueueTask = {
      id: activeConfirmation.taskId, name: t('download.multipleFilesTask'), kind: 'folder', status: 'queued', progress: 0, speed: t('download.waitingToStart'),
      children: activeConfirmation.resources.filter((resource) => selectedIds.includes(resource.id)).map((resource) => ({ id: resource.id, name: resource.name, kind: 'file', status: 'queued', progress: 0, size: resource.size }))
    };
    const nativeTask = await (destinationDirectory
      ? confirmDownload(activeConfirmation.taskId, selectedIds, destinationDirectory, privateDownload)
      : confirmDownload(activeConfirmation.taskId, selectedIds, undefined, privateDownload)).catch(() => undefined);
    const confirmed = nativeTask ?? task;
    addNewDownloadTask(confirmed); setSelectedTaskId(confirmed.id); setActiveConfirmation(null);
  }

  async function taskCommand(id: string, command: TaskCommand) {
    const task = findQueueTask(tasks, id);
    if (!task) return;
    if (command === 'details') { setSelectedTaskId(id); setDetailsTaskId(id); return; }
    if (command === 'delete') { setPendingPurgeTaskIds([id]); return; }
    const optimisticStart = command === 'resume' || command === 'retry';
    if (optimisticStart) {
      if (pendingStartTaskIds.current.has(id)) return;
      queueTaskOptimistically(task);
    }
    try {
      if (command === 'reauthorize') {
        if (taskAuthorizationActive.current) return;
        setNotice({ kind: 'success', message: t('cloud.authorization.opening') });
        taskAuthorizationActive.current = true;
        const generation = ++taskAuthorizationGeneration.current;
        let authorization: CloudAuthorization;
        try { authorization = await cloudApi.reauthorizeFailedCloudTask(id); }
        catch (error) {
          if (generation !== taskAuthorizationGeneration.current) return;
          taskAuthorizationActive.current = false;
          throw error;
        }
        if (generation !== taskAuthorizationGeneration.current) return;
        setTaskReauthorization(authorization.completed ? null : authorization);
        if (authorization.completed) {
          taskAuthorizationActive.current = false;
          await refreshTaskSnapshot(() => false, 'active');
          await refreshTaskSnapshot(() => false, 'terminal');
        }
        setNotice({ kind: 'success', message: authorization.instructions });
        return;
      }
      if (command === 'move_to_private' || command === 'move_to_normal') {
        const updated = await setTaskPrivacy(id, command === 'move_to_private');
        if (updated) {
          const projected = applyTaskProjection(updated);
          if (command === 'move_to_private' && !privacyVisibility.unlocked && isPrivateTask(projected)) {
            setSelectedTaskIds((current) => current.filter((selectedId) => selectedId !== id));
            setDetailsTaskId((current) => current === id ? null : current);
          }
        }
        return;
      }
      if (command === 'open_directory') {
        await openTaskDirectory(id);
        return;
      }
      if (command === 'open_file') {
        await openTaskFile(id);
        return;
      }
      if (command === 'play_videos') {
        await playTaskVideos(id);
        return;
      }
      if (command === 'copy_link') {
        if (!task.source) throw new Error(t('download.error.noSourceToCopy'));
        await writeClipboardText(task.source);
        setNotice({ kind: 'success', message: t('download.linkCopied') });
        return;
      }
      const result = await runTaskAction(id, command);
      if (result.duplicate) {
        const projection = result.queueTask ?? result.duplicate.task;
        applyTaskProjection(projection);
        setSelectedTaskId(id);
        enqueueDuplicate(result.duplicate);
      } else if (result.queueTask) {
        applyTaskProjection(result.queueTask);
      } else if (result.task) {
        applyTaskProjection(result.task);
      } else if (optimisticStart) {
        void refreshTaskSnapshot(() => false, 'active');
      }
    } catch (error) {
      const message = optimisticStart
        ? restoreTaskAfterStartFailure(task, error)
        : error && typeof error === 'object' && 'message' in error && typeof error.message === 'string'
          ? error.message : String(error);
      setNotice({ kind: 'error', message });
    } finally {
      if (optimisticStart) pendingStartTaskIds.current.delete(id);
    }
  }

  async function writeClipboardText(value: string) {
    if (isTauriRuntime()) {
      const { writeText } = await import('@tauri-apps/plugin-clipboard-manager');
      await writeText(value);
      return;
    }
    await navigator.clipboard.writeText(value);
  }

  async function batchTaskCommand(ids: readonly string[], command: BatchTaskCommand) {
    const uniqueIds = [...new Set(ids)];
    const targets = uniqueIds.map((id) => findQueueTask(tasks, id)).filter((task): task is QueueTask => Boolean(task));
    if (!targets.length) return;
    try {
      if (command === 'copy_link') {
        const links = targets.map((task) => task.source?.trim()).filter((source): source is string => Boolean(source));
        if (!links.length) throw new Error(t('download.error.noSelectedSourcesToCopy'));
        await writeClipboardText(links.join('\n'));
        setNotice({ kind: 'success', message: t('download.linksCopied', { count: links.length }) });
        return;
      }
      if (command === 'delete') {
        setPendingPurgeTaskIds(targets.map((task) => task.id));
        return;
      }

      const privacyTarget = command === 'move_to_private' ? true : command === 'move_to_normal' ? false : undefined;

      type BatchTaskOperation = { task: QueueTask; optimisticStart?: boolean; run: () => Promise<QueueTask | undefined> };
      const operations: BatchTaskOperation[] = targets.flatMap<BatchTaskOperation>((task) => {
        if (privacyTarget !== undefined) return isPrivateTask(task) === privacyTarget
          ? []
          : [{ task, run: () => setTaskPrivacy(task.id, privacyTarget) }];
        if (command === 'pause') return ['downloading', 'queued'].includes(task.status)
          ? [{ task, run: () => runTaskAction(task.id, 'pause').then((result) => result.queueTask ?? result.task) }]
          : [];
        if (command === 'resume') {
          if (task.status === 'paused' && !pendingStartTaskIds.current.has(task.id)) return [{ task, optimisticStart: true, run: () => runTaskAction(task.id, 'resume').then((result) => result.queueTask ?? result.task) }];
          if (task.status === 'failed' && !pendingStartTaskIds.current.has(task.id)) return [{ task, optimisticStart: true, run: () => runTaskAction(task.id, 'retry').then((result) => result.queueTask ?? result.task) }];
          return [];
        }
        return [];
      });
      operations.filter((operation) => operation.optimisticStart).forEach(({ task }) => queueTaskOptimistically(task));
      const results = await settleTaskOperations(operations, command === 'pause');
      let failure: unknown;
      const newlyHiddenIds = new Set<string>();
      let refreshNeeded = false;
      results.forEach((result, index) => {
        const operation = operations[index];
        if (result.status === 'rejected') {
          failure ??= result.reason;
          if (operation.optimisticStart) {
            restoreTaskAfterStartFailure(operation.task, result.reason);
            pendingStartTaskIds.current.delete(operation.task.id);
          }
          return;
        }
        if (result.value) {
          const projected = applyTaskProjection(result.value);
          if (privacyTarget === true && !privacyVisibility.unlocked && isPrivateTask(projected)) newlyHiddenIds.add(projected.id);
        } else if (operation.optimisticStart) {
          refreshNeeded = true;
        }
        if (operation.optimisticStart) pendingStartTaskIds.current.delete(operation.task.id);
      });
      if (newlyHiddenIds.size) {
        setSelectedTaskIds((current) => current.filter((id) => !newlyHiddenIds.has(id)));
        setDetailsTaskId((current) => current && newlyHiddenIds.has(current) ? null : current);
      }
      if (failure) throw failure;
      if (refreshNeeded) void refreshTaskSnapshot(() => false, 'active');
    } catch (error) {
      setNotice({ kind: 'error', message: error instanceof Error ? error.message : String(error) });
    }
  }

  async function selectTaskFile(taskId: string, fileIndex: number, selected: boolean) {
    const task = findQueueTask(tasks, taskId);
    const files = task?.files;
    if (!task || !files?.some((file) => file.index === fileIndex)) return;
    const wasSelected = files.some((file) => file.index === fileIndex && file.selected);
    if (selected && wasSelected && task.status === 'paused') {
      await taskCommand(taskId, 'resume');
      return;
    }
    const selectedIndices = files
      .filter((file) => file.index === fileIndex ? selected : file.selected)
      .map((file) => file.index);
    if (!selectedIndices.length) {
      await taskCommand(taskId, 'pause');
      return;
    }
    try {
      const updated = await selectDownloadFiles(taskId, selectedIndices);
      if (updated) {
        applyTaskProjection(updated);
      }
      if (selected && task.status === 'paused') await taskCommand(taskId, 'resume');
    } catch (error) {
      setNotice({ kind: 'error', message: error instanceof Error ? error.message : String(error) });
    }
  }

  async function cancelTaskFile(taskId: string, fileIndex: number) {
    try {
      const updated = await cancelDownloadFile(taskId, fileIndex);
      if (updated) {
        applyTaskProjection(updated);
      }
    } catch (error) {
      setNotice({ kind: 'error', message: error instanceof Error ? error.message : String(error) });
    }
  }

  async function confirmTaskRemoval(deleteFiles: boolean) {
    const ids = pendingPurgeTaskIds ?? [];
    if (!ids.length || purgeInProgressRef.current) return;
    const removeTasksFromQueue = (taskIds: readonly string[]) => {
      const removed = new Set(taskIds);
      tasksRef.current = tasksRef.current.filter((task) => !removed.has(task.id));
      setTasks(tasksRef.current);
      setSelectedTaskIds((current) => current.filter((id) => !removed.has(id)));
      setDetailsTaskId((current) => current && removed.has(current) ? null : current);
    };
    const successMessage = deleteFiles ? t('download.removedWithFiles') : t('download.removedKeepingFiles');
    purgeInProgressRef.current = true;
    for (const id of ids) purgingTaskIds.current.add(id);
    protectRemovedTasksFromOlderSnapshots(ids);
    setPendingPurgeTaskIds(null);
    setNotice({ kind: 'success', message: t('download.removingProgress', { removed: 0, total: ids.length }) });

    let removed = 0;
    let timer: number | undefined;
    const removeVisibleTasks = () => {
      const batch = ids.slice(removed, removed + backgroundPurgeFrameSize);
      if (!batch.length) return;
      removeTasksFromQueue(batch);
      removed += batch.length;
      setNotice({ kind: 'success', message: t('download.removingProgress', { removed, total: ids.length }) });
      if (removed < ids.length) {
        timer = window.setTimeout(removeVisibleTasks, backgroundPurgeFrameDelayMs);
      }
    };
    const removeRemainingVisibleTasks = () => {
      if (timer !== undefined) window.clearTimeout(timer);
      timer = undefined;
      if (removed < ids.length) {
        removeTasksFromQueue(ids.slice(removed));
        removed = ids.length;
      }
    };

    const purge = deleteTasks(ids, deleteFiles);
    removeVisibleTasks();
    void (async () => {
      try {
        await purge;
        removeRemainingVisibleTasks();
        await refreshTaskSnapshot(() => false, 'terminal');
        for (const id of ids) {
          permanentlyPurgedTaskIds.current.add(id);
          purgingTaskIds.current.delete(id);
        }
        purgeInProgressRef.current = false;
        setNotice({ kind: 'success', message: successMessage });
      } catch (error) {
        if (timer !== undefined) window.clearTimeout(timer);
        timer = undefined;
        await refreshTaskSnapshot(() => false, 'terminal', false);
        for (const id of ids) purgingTaskIds.current.delete(id);
        purgeInProgressRef.current = false;
        setNotice({ kind: 'error', message: error instanceof Error ? error.message : String(error) });
      }
    })();
  }

  function submitHeaderInput() {
    const input = searchInput.trim();
    if (!input) { setHeaderPanel(null); return; }
    if (hasMultipleLines(searchInput) || isDownloadInput(input)) {
      setHeaderPanel(null);
      setSearchInput('');
      setNewDownloadInput(input);
      setNewDownloadOpen(true);
      return;
    }
    setHeaderPanel('search');
  }

  function selectSearchResult(task: QueueTask) {
    setView('downloads');
    setDownloadTab(taskTabForSearchResult(task));
    setSelectedTaskId(task.id);
    setHeaderPanel(null);
    setSearchInput('');
    window.setTimeout(() => {
      Array.from(document.querySelectorAll<HTMLElement>('[data-task-id]'))
        .find((element) => element.dataset.taskId === task.id)
        ?.scrollIntoView?.({ block: 'center', behavior: 'smooth' });
    }, 0);
  }

  function selectDownloadTab(tab: TaskListTab) {
    setDownloadTab(tab);
    setSelectedTaskId(null);
  }

  function acceptCloudDownload(value: unknown) {
    const outcome = value as TaskActionResult | undefined;
    if (outcome?.duplicate) {
      const task = outcome.queueTask ?? outcome.duplicate.task;
      applyTaskProjection(task);
      setSelectedTaskId(outcome.duplicate.task.id);
      enqueueDuplicate(outcome.duplicate);
      return;
    }
    if (outcome?.queueTask) {
      addNewDownloadTask(outcome.queueTask);
      setSelectedTaskId(outcome.queueTask.id);
      setNotice({ kind: 'success', message: t('download.addedToQueue', { name: outcome.queueTask.name }) });
      return;
    }
    const task = outcome?.task ?? value;
    if (!isQueueTask(task)) {
      setNotice({ kind: 'error', message: t('download.error.invalidCloudResponse') });
      return;
    }
    addNewDownloadTask(task);
    setSelectedTaskId(task.id);
    setNotice({ kind: 'success', message: t('download.addedToQueue', { name: task.name }) });
  }

  async function submitPreparedBatch(
    items: DownloadDraftItem[],
    destinationDirectory?: string,
    privateDownload = false,
    postProcessingNote?: string,
  ) {
    let firstCreated = false;
    const accepted = new Set<string>();
    const acceptCreated = (task: QueueTask, newlyCreated = true) => {
      if (newlyCreated) addNewDownloadTask(task);
      else applyTaskProjection(task);
      if (accepted.has(task.id)) return;
      accepted.add(task.id);
      if (firstCreated) return;
      firstCreated = true;
      setNewDownloadOpen(false);
      setNewDownloadInput('');
      setHeaderPanel(null);
      setSearchInput('');
      setView('downloads');
      setDownloadTab('active');
      setSelectedTaskId(task.id);
      window.setTimeout(() => {
        Array.from(document.querySelectorAll<HTMLElement>('[data-task-id]'))
          .find((element) => element.dataset.taskId === task.id)
          ?.scrollIntoView?.({ block: 'center', behavior: 'smooth' });
      }, 0);
    };

    const outcomes = await Promise.allSettled(items.map(async (item) => {
      if (item.result?.duplicate) {
        acceptCreated(item.result.duplicate.task, false);
        enqueueDuplicate(item.result.duplicate);
        return;
      }
      if (item.result?.task) {
        acceptCreated(item.result.task);
        return;
      }
      const prepared = item.result?.confirmation;
      if (!prepared) throw new Error(t('download.error.taskNotParsed', { input: item.input }));
      const ids = selectedResourceIds(item);
      if (prepared.taskId.startsWith('cloud-share://')) {
        const result = await confirmCloudShareDownload(
          prepared.taskId,
          ids,
          destinationDirectory,
          privateDownload,
          acceptCreated,
          postProcessingNote,
        );
        for (const outcome of result.outcomes) {
          const task = outcome.queueTask ?? outcome.task ?? outcome.duplicate?.task;
          if (task) acceptCreated(task, !outcome.duplicate);
          if (outcome.duplicate) enqueueDuplicate(outcome.duplicate);
        }
        if (result.failures.length) throw new Error(result.failures.map(failure => failure.message).join('；'));
        return;
      }
      const task = await confirmDownload(
        prepared.taskId,
        ids,
        destinationDirectory,
        privateDownload,
        postProcessingNote,
      );
      if (!task) throw new Error(t('download.error.taskSubmissionFailed', { name: prepared.title || item.input }));
      acceptCreated(task);
    }));

    const failures = outcomes.flatMap((outcome) => {
      if (outcome.status !== 'rejected') return [];
      const reason = outcome.reason;
      const message = reason instanceof Error
        ? reason.message
        : typeof reason === 'object' && reason !== null && 'message' in reason && typeof reason.message === 'string'
          ? reason.message
          : String(reason);
      return [message];
    });
    if (failures.length) {
      const message = t('download.error.tasksSubmissionFailed', { count: failures.length, detail: failures[0] });
      setNotice({ kind: 'error', message });
      if (!firstCreated) throw new Error(message);
    }
    if (!firstCreated) throw new Error(t('download.error.noTaskAdded'));
  }

  return (
    <main className="app-shell" style={{ WebkitUserSelect: 'none', userSelect: 'none' }}>
      <header className="app-header" data-native-window-drag={nativeWindowDrag || undefined}>
        <div className="app-header-drag-space" data-tauri-drag-region={nativeWindowDrag ? undefined : true} aria-hidden="true" />
        <div className="brand" data-tauri-drag-region={nativeWindowDrag ? undefined : 'deep'}>
          <div className="brand-mark" data-icon-variant={resolvedTheme} aria-hidden="true"><img src={resolvedTheme === 'dark' ? darkMintIcon : mintWhiteIcon} alt="" draggable={false} /></div>
          <h1>{t('app.brand')}</h1>
          <button type="button" className="sidebar-toggle" data-window-no-drag aria-label={sidebarCollapsed ? t('navigation.expandSidebar') : t('navigation.collapseSidebar')} aria-controls="primary-navigation" aria-expanded={!sidebarCollapsed} title={sidebarCollapsed ? t('navigation.expandSidebar') : t('navigation.collapseSidebar')} onClick={() => setSidebarCollapsed((collapsed) => !collapsed)}><SidebarToggleIcon collapsed={sidebarCollapsed} /></button>
        </div>
        <div className="header-input-shell" data-window-no-drag>
          <label ref={searchDropTarget} className={`global-search${headerPanel === 'search' ? ' has-search-results' : ''}`}>
            <span aria-hidden="true">⌕</span>
            <textarea
              style={{ WebkitUserSelect: 'text', userSelect: 'text' }}
              rows={1}
              wrap="off"
              aria-label={t('search.label')}
              placeholder={t('search.placeholder')}
              disabled={newDownloadOpen || Boolean(detailsTask)}
              value={searchInput}
              onChange={(event) => {
                const next = event.target.value;
                setSearchInput(next);
                setSelectedSearchResultIndex(null);
                const query = next.trim();
                setHeaderPanel(query && !hasMultipleLines(next) && !isDownloadInput(query) ? 'search' : null);
              }}
              onPaste={(event) => {
                const pasted = event.clipboardData.getData('text');
                if (!pasted.trim()) return;
                event.preventDefault();
                setSearchInput('');
                setHeaderPanel(null);
                setSelectedSearchResultIndex(null);
                setNewDownloadInput(pasted);
                setNewDownloadOpen(true);
              }}
              onKeyDown={(event) => {
                if (event.key === 'Escape' && headerPanel === 'search') {
                  event.preventDefault();
                  setHeaderPanel(null);
                  setSelectedSearchResultIndex(null);
                  return;
                }
                if ((event.key === 'ArrowDown' || event.key === 'ArrowUp') && headerPanel === 'search' && taskSearchResults.length) {
                  event.preventDefault();
                  const moveDown = event.key === 'ArrowDown';
                  setSelectedSearchResultIndex((current) => moveDown
                    ? Math.min((current ?? -1) + 1, taskSearchResults.length - 1)
                    : Math.max((current ?? taskSearchResults.length) - 1, 0));
                  return;
                }
                if (event.key === 'Enter' && !event.shiftKey && headerPanel === 'search' && selectedSearchResultIndex !== null) {
                  const selectedTask = taskSearchResults[selectedSearchResultIndex];
                  if (selectedTask) {
                    event.preventDefault();
                    selectSearchResult(selectedTask);
                    return;
                  }
                }
                if (event.key === 'Enter' && !event.shiftKey) {
                  event.preventDefault();
                  submitHeaderInput();
                }
              }}
            />
            {headerPanel === 'search' && <button type="button" className="global-search-clear" aria-label={t('search.clear')} onClick={(event) => {
              event.preventDefault();
              setSearchInput('');
              setHeaderPanel(null);
              setSelectedSearchResultIndex(null);
            }}>×</button>}
          </label>
          {(headerPanel || headerConfirmation) && <div className={`header-input-panel${headerPanel === 'search' ? ' header-search-panel' : ''}`} data-window-no-drag>
            {headerConfirmation ? <ParseConfirmDialog placement="header" result={headerConfirmation} downloadDirectory={settings.downloadDirectory} browseDirectory={browseDownloadDirectory} privacyEnabled={settings.privacyDownloadEnabled ?? false} onConfirm={(selectedIds, destinationDirectory, privateDownload) => void confirm(selectedIds, destinationDirectory, privateDownload, 'header')} onSelectionChange={(selectedIds) => setHeaderConfirmation((current) => current ? {
              ...current,
              resources: current.resources.map((resource) => ({ ...resource, selected: selectedIds.includes(resource.id) }))
            } : current)} onClose={() => { dismissHeaderConfirmation(); setHeaderPanel(null); }} />
              : headerPanel === 'download' ? <NewDownloadDialog open placement="header" initialInput={searchInput} onScan={scanDownloadInputs} onParse={async (input, accessCode) => {
                const result = await parse(input, accessCode, 'header');
                setView('downloads');
                setDownloadTab('active');
                return result;
              }} onClose={() => { setHeaderPanel(null); setSearchInput(''); }} />
                : <section className="header-search-results" role="region" aria-label={t('search.results')}>
                  <header><strong>{t('search.results')}</strong><span>{t('search.taskCount', { count: taskSearchResults.length })}</span></header>
                  {taskSearchResults.length ? <div>{taskSearchResults.map((task, index) => <button type="button" key={task.id} className={`header-search-result${selectedSearchResultIndex === index ? ' is-keyboard-selected' : ''}`} aria-label={t('search.openTask', { name: displayTaskName(task) })} onClick={() => selectSearchResult(task)}>
                    <span className={`header-search-status ${task.status === 'failed' ? 'failed' : taskTabForSearchResult(task)}`}>{taskStatusLabel(task)}</span>
                    <strong title={displayTaskName(task)}>{displayTaskName(task)}</strong>
                  </button>)}</div> : <p>{t('search.noMatches')}</p>}
                </section>}
          </div>}
        </div>
        <div className="window-controls" aria-label={t('window.controls')} style={{ marginLeft: 'auto', flex: '0 0 auto' }}>
          <button aria-label={t('window.minimize')} onClick={() => void runWindowAction('minimize')}><WindowIcon kind="minimize" /></button>
          <button aria-label={t('window.maximizeRestore')} onClick={() => void runWindowAction('maximize')}><WindowIcon kind="maximize" /></button>
          <button className="window-close" aria-label={t('dialog.close')} onClick={() => void runWindowAction('hide')}><WindowIcon kind="close" /></button>
        </div>
      </header>
      <EngineStatusToast state={engineState} onRetry={() => {
        if (engineState.retrying) return;
        setEngineState((current) => ({ ...current, retrying: true, error: undefined }));
        void retryDownloadEngine().then(setEngineState).catch((error: unknown) => setEngineState({ available: false, retrying: false, error: String(error) }));
      }} />
      <div className={`workspace${sidebarCollapsed ? ' sidebar-collapsed' : ''}`}><nav id="primary-navigation" aria-label={t('navigation.primary')} aria-hidden={sidebarCollapsed}>
        <button className={view === 'downloads' ? 'nav-active' : ''} onClick={() => setView('downloads')}><span className="nav-icon" aria-hidden="true"><NavigationIcon kind="tasks" /></span><span>{t('navigation.tasks')}</span>{activeTasks.length > 0 && <b>{activeTasks.length}</b>}</button>
        <CloudNavigation providers={authorizedCloudProviders} activePage={view === 'cloud' ? cloudPage : null} icon={<NavigationIcon kind="cloud" />} onSelect={page => { setCloudPage(page); setView('cloud'); }} />
        <button className={view === 'settings' ? 'nav-active' : ''} onClick={() => setView('settings')}><span className="nav-icon" aria-hidden="true"><NavigationIcon kind="settings" /></span><span>{t('navigation.settings')}</span></button>
        <button type="button" className={`nav-about${view === 'about' ? ' nav-active' : ''}`} aria-current={view === 'about' ? 'page' : undefined} onClick={() => setView('about')}><span className="nav-icon" aria-hidden="true"><NavigationIcon kind="about" /></span><span>{appUpdate ? t('navigation.newVersion') : t('navigation.about')}</span></button>
      </nav>
        {view === 'downloads' && <section className="downloads-view">
          <header className="download-tabs">
            <div className="download-tab-list workspace-tab-list" role="tablist" aria-label={t('download.taskList')}>
              <button role="tab" aria-selected={downloadTab === 'all'} onClick={() => selectDownloadTab('all')}>{t('download.tabs.all')} <b>{allTasks.length}</b></button>
              <button role="tab" aria-selected={downloadTab === 'active'} onClick={() => selectDownloadTab('active')}>{t('download.tabs.active')} <b>{activeTasks.length}</b></button>
              <button role="tab" aria-selected={downloadTab === 'completed'} onClick={() => selectDownloadTab('completed')}>{t('download.tabs.completed')} <b>{completedTasks.length}</b></button>
              <button role="tab" aria-selected={downloadTab === 'failed'} onClick={() => selectDownloadTab('failed')}>{t('download.tabs.failed')} <b>{failedTasks.length}</b></button>
            </div>
            <div className="speed-summary" aria-label={t('download.currentSpeed')}><strong>{currentSpeed}</strong></div>
            <div className="task-privacy-actions">
              {privacyVisibility.enabled && <button type="button" className="task-privacy-lock" aria-label={privacyVisibility.unlocked ? t('privacy.lock') : t('privacy.unlock')} title={privacyVisibility.unlocked ? t('privacy.lock') : t('privacy.unlock')} onClick={() => void togglePrivacyVisibility()}><PrivacyLockIcon unlocked={privacyVisibility.unlocked} /></button>}
              <TaskSortControl tab={downloadTab} value={taskSorts[downloadTab]} onChange={(mode) => setTaskOrderPreferences((current) => ({
                sorts: { ...current.sorts, [downloadTab]: mode },
                orders: mode === 'manual' && !current.orders[downloadTab].length
                  ? { ...current.orders, [downloadTab]: visibleTasks.map((task) => task.id) }
                  : current.orders,
              }))} privacyFilter={privacyTaskFilter} onPrivacyFilterChange={setPrivacyTaskFilter} showPrivacyFilter={privacyVisibility.unlocked} />
            </div>
          </header>
          <section className="queue-area">
            <DownloadQueue tasks={visibleTasks} privacyEnabled={settings.privacyDownloadEnabled === true} selectedTaskId={selectedTaskId} selectedTaskIds={selectedTaskIds} onSelect={() => {}} onSelectionChange={(ids) => setSelectedTaskIds([...ids])} onCommand={taskCommand} onBatchCommand={batchTaskCommand} onReorder={(orderedTaskIds) => setTaskOrderPreferences((current) => ({
              sorts: { ...current.sorts, [downloadTab]: 'manual' },
              orders: {
                ...current.orders,
                [downloadTab]: mergeVisibleTaskOrder(
                  current.orders[downloadTab].length ? current.orders[downloadTab] : visibleTasks.map((task) => task.id),
                  visibleTasks.map((task) => task.id),
                  orderedTaskIds,
                ),
              },
            }))} onFileSelection={selectTaskFile} onFileCancel={cancelTaskFile} />
          </section>
        </section>}
        <section className="view-panel" hidden={view !== 'cloud'}><CloudWorkspace client={cloudClient} activePage={cloudPage} onPageChange={setCloudPage} onProvidersChange={setAuthorizedCloudProviders} onDownloadTask={acceptCloudDownload} privacyDownloadEnabled={settings.privacyDownloadEnabled ?? false} /></section>
        {view === 'settings' && <section className="view-panel"><SettingsPanel initial={settings} onChange={saveSettings} browseDirectory={browseDownloadDirectory} saveAiSettings={saveAiConfiguration} loadAiModels={listAiModels} onNotice={setNotice} configurePrivacyDownload={savePrivacyConfiguration} /></section>}
        {view === 'about' && <section className="view-panel" aria-label={t('about.title')}><AboutPanel icon={resolvedTheme === 'dark' ? darkMintIcon : mintWhiteIcon} update={appUpdate} onOpenUpdate={(update) => { void openExternalUrl(update.downloadUrl).catch(() => undefined); }} /></section>}
      </div>
      {confirmation && <ParseConfirmDialog result={confirmation} downloadDirectory={settings.downloadDirectory} browseDirectory={browseDownloadDirectory} privacyEnabled={settings.privacyDownloadEnabled ?? false} onConfirm={confirm} onSelectionChange={(selectedIds) => setConfirmation((current) => current ? {
        ...current,
        resources: current.resources.map((resource) => ({ ...resource, selected: selectedIds.includes(resource.id) }))
      } : current)} onClose={dismissConfirmation} />}
      {privacyUnlockOpen && <div className="modal-backdrop" role="presentation"><section className="cloud-trash-confirm" role="dialog" aria-modal="true" aria-labelledby="privacy-unlock-title"><h3 id="privacy-unlock-title">{t('privacy.unlockTitle')}</h3><form onSubmit={(event) => { event.preventDefault(); void unlockPrivateTasks(); }}><div className="cloud-operation-editor"><label><input ref={privacyUnlockPassword} type="password" autoComplete="current-password" aria-label={t('settings.privacy.password')} disabled={privacyUnlocking} /></label></div>{privacyUnlockError && <p className="cloud-operation-status error" role="alert">{privacyUnlockError}</p>}<div className="new-download-actions"><button type="button" onClick={() => setPrivacyUnlockOpen(false)} disabled={privacyUnlocking}>{t('dialog.cancel')}</button><button className="primary" type="submit" disabled={privacyUnlocking}>{privacyUnlocking ? t('privacy.unlocking') : t('privacy.unlockAction')}</button></div></form></section></div>}
      {duplicate && <DuplicateDownloadDialog
        task={duplicate.task}
        state={duplicate.state}
        onOpenDirectory={() => void openDuplicateDirectory()}
        onResolve={(action) => void resolveDuplicate(action)}
        onRestartMissing={() => void resolveDuplicate('restart_missing')}
        onClose={() => dismissDuplicate(duplicate.task.id)}
      />}
      <NewDownloadDialog open={newDownloadOpen} placement="modal" initialInput={newDownloadInput} onScan={scanDownloadInputs} onParse={(input, accessCode) => parseDownload(input, accessCode, true)} onDiscardPrepared={discardPreparedDownload} onSubmitBatch={submitPreparedBatch} downloadDirectory={settings.downloadDirectory} browseDirectory={browseDownloadDirectory} privacyEnabled={settings.privacyDownloadEnabled ?? false} onClose={() => { setNewDownloadOpen(false); setNewDownloadInput(''); }} />
      {detailsTask && <TaskDetailsDialog task={detailsTask} onClose={() => setDetailsTaskId(null)} />}
      {pendingPurgeTasks.length > 0 && <RemoveTaskDialog taskNames={pendingPurgeTasks.map(displayTaskName)} onConfirm={(deleteFiles) => void confirmTaskRemoval(deleteFiles)} onClose={() => setPendingPurgeTaskIds(null)} />}
      {notice && <div className={`task-toast ${notice.kind}`} role="status">{notice.message}</div>}
    </main>
  );
}
