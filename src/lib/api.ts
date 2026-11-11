import { AiSettings, AiSettingsInput, AppSettings, AppUpdate, CloudShareAccessCodeRequired, CloudShareBatchFailure, CloudShareBatchResult, ConfirmResource, DownloadEngineState, DownloadGlobalStat, DuplicateDownloadState, ParseDownloadResult, PrivacyTaskVisibility, QueueTask, ScanDownloadResult, TaskActionResult, TaskCommand } from './types';
import { videoConfirmation, videoMessage } from './webVideo';
import type { VideoPreview } from './types';
import { Channel, invoke } from '@tauri-apps/api/core';
import { version as appVersion } from '../../src-tauri/tauri.conf.json';
import type { ThemeId } from './theme';
import { t, type TranslationKey } from '../i18n';
import { isXunleiVerificationFailure, isXunleiVerificationReason, xunleiVerificationMessage } from './xunleiVerification';

function fileNameFrom(value: string) {
  if (/^ed2k:\/\/\|file\|/i.test(value)) {
    return decodeURIComponent(value.split('|')[2] || 'ED2K 下载');
  }
  const path = value.split(/[?#]/)[0];
  return decodeURIComponent(path.split('/').filter(Boolean).pop() || '新下载任务');
}

function sourceForDisplay(value: string, kind: string) {
  if (kind !== 'ftp') return value;
  try {
    const parsed = new URL(value);
    parsed.username = '';
    parsed.password = '';
    return parsed.toString();
  } catch {
    return value;
  }
}

function id() { return `local-${Date.now()}-${Math.random().toString(16).slice(2)}`; }

export function parseLocally(input: string): ParseDownloadResult {
  const trimmed = input.trim();
  if (/^https?:\/\/(?:(?:www\.|m\.|music\.)?youtube\.com\/(?:watch\?|shorts\/|embed\/|live\/)|youtu\.be\/|(?:www\.|m\.)?bilibili\.com\/video\/|b23\.tv\/)/i.test(trimmed)) throw new Error(t('video.error.desktop'));
  const isMulti = /^(magnet:|thunder:)/i.test(trimmed) || /\.torrent(?:\?|$)/i.test(trimmed) || /(?:pan\.quark\.cn|mypikpak\.com|pikpak\.me)/i.test(trimmed);
  if (isMulti) {
    const resources: ConfirmResource[] = [
      { id: `${id()}-1`, name: '下载内容 01', size: '待连接后获取', selected: true },
      { id: `${id()}-2`, name: '下载内容 02', size: '待连接后获取', selected: true }
    ];
    return { confirmation: { taskId: id(), title: 'BT 下载任务', requiresConfirmation: true, resources, processing: ['完成后按白名单检查后处理步骤'] } };
  }
  if (/^(?:https?|ftp):\/\//i.test(trimmed) || /^ed2k:\/\/\|file\|/i.test(trimmed)) {
    const kind = /^ed2k:/i.test(trimmed) ? 'ed2k'
      : /^ftp:/i.test(trimmed) ? 'ftp'
        : /\.m3u8(?:[?#]|$)/i.test(trimmed) ? 'hls'
          : /\.mpd(?:[?#]|$)/i.test(trimmed) ? 'dash'
            : 'file';
    return { task: { id: id(), name: fileNameFrom(trimmed), kind, status: 'downloading', progress: 0, speed: '准备连接', source: sourceForDisplay(trimmed, kind) } };
  }
  return { confirmation: { taskId: id(), title: '待确认的下载内容', requiresConfirmation: true, resources: [{ id: `${id()}-candidate`, name: '从文本中发现的候选下载项', size: '需要进一步解析', selected: true }], processing: ['将用本地规则提取链接；必要时请求脱敏 AI 分析'] } };
}

export function isTauriRuntime() {
  return typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;
}

export async function syncMainWindowIcon(theme: ThemeId): Promise<void> {
  if (!isTauriRuntime()) return;
  await invoke('set_main_window_icon', { theme });
}

type NativeTaskOutcome = QueueTask & { duplicate?: DuplicateDownloadState; queueTask?: QueueTask };
type NativeCloudShareBatchResult = { outcomes: NativeTaskOutcome[]; failures: CloudShareBatchFailure[] };
type NativeTaskParseOutcome = {
  kind: 'task';
  task: QueueTask;
  requires_confirmation: boolean;
  resources?: ConfirmResource[];
  duplicate?: DuplicateDownloadState;
};

type NativeShareConfirmationOutcome = {
  kind: 'share_confirmation';
  plan_source: string;
  title: string;
  resources: ConfirmResource[];
};

type NativeAccessCodeRequiredOutcome = {
  kind: 'access_code_required';
  provider: CloudShareAccessCodeRequired['provider'];
  message: string;
};

type NativeParseOutcome = NativeTaskParseOutcome | NativeShareConfirmationOutcome | NativeAccessCodeRequiredOutcome | ({ kind: 'web_video' } & VideoPreview);

function duplicateResult(result: NativeTaskOutcome): TaskActionResult {
  const { duplicate, queueTask, ...task } = result;
  return duplicate
    ? { duplicate: { task, state: duplicate }, ...(queueTask ? { queueTask } : {}) }
    : { task, ...(queueTask ? { queueTask } : {}) };
}

export async function parseDownload(input: string, accessCode?: string, requireConfirmation = false, probeVideo = false): Promise<ParseDownloadResult> {
  if (!isTauriRuntime()) {
    if (probeVideo) throw new Error(t('video.error.desktop'));
    return parseLocally(input);
  }
  const result = await invoke<NativeParseOutcome>('parse_download', { input, accessCode, ...(requireConfirmation ? { requireConfirmation } : {}), ...(probeVideo ? { probeVideo } : {}) })
    .catch((error: unknown) => {
      if (isXunleiVerificationReason(error) || isXunleiVerificationFailure(error)) {
        throw new Error(xunleiVerificationMessage(typeof error === 'string' ? error : (error as { message?: unknown }).message));
      }
      if (typeof error === 'string' && error.startsWith('video.error.')) throw new Error(videoMessage(error));
      throw error;
    });
  if (result.kind === 'web_video') return { confirmation: videoConfirmation(result) };
  if (result.kind === 'share_confirmation') {
    return {
      confirmation: {
        taskId: result.plan_source,
        title: result.title,
        requiresConfirmation: true,
        resources: result.resources,
        processing: []
      }
    };
  }
  if (result.kind === 'access_code_required') {
    return {
      shareAccessCodeRequired: {
        provider: result.provider,
        message: result.message
      }
    };
  }
  if (result.duplicate) return { duplicate: { task: result.task, state: result.duplicate } };
  if (!result.requires_confirmation) return { task: result.task };
  const resources = result.resources ?? [];
  const isBitTorrent = result.task.kind === 'magnet' || result.task.kind === 'torrent';
  return {
    confirmation: {
      taskId: result.task.id,
      title: result.task.name,
      requiresConfirmation: true,
      resources,
      processing: ['文件选择确认后调用本地 aria2 下载'],
      isBitTorrent,
      fileListPending: isBitTorrent && resources.length === 0,
    }
  };
}

export async function discardPreparedDownload(taskId: string): Promise<void> {
  if (!isTauriRuntime()) return;
  if (taskId.startsWith('video-page:')) {
    await invoke('discard_video_preview', { previewId: taskId.slice('video-page:'.length) });
    return;
  }
  await invoke('discard_prepared_download', { taskId });
}

export async function scanDownloadInputs(input: string): Promise<ScanDownloadResult[]> {
  if (!isTauriRuntime()) {
    const source = input.trim();
    try {
      const url = new URL(source);
      if (url.hostname && (url.protocol === 'http:' || url.protocol === 'https:' || url.protocol === 'ftp:')) {
        const path = url.pathname.toLowerCase();
        const kind = url.protocol === 'ftp:' ? 'ftp'
          : path.endsWith('.m3u8') ? 'hls'
            : path.endsWith('.mpd') ? 'dash'
              : path.endsWith('.torrent') ? 'torrent'
                : 'direct';
        return [{ input: source, valid: true, kind }];
      }
    } catch {
      // The native parser is authoritative. The browser fallback only keeps
      // ordinary URL-based workflows usable in tests and web previews.
    }
    return [{ input: source, valid: false, message: '未发现受支持的有效下载链接' }];
  }
  return invoke<ScanDownloadResult[]>('scan_download_inputs', { input });
}

function encodeBase64(bytes: Uint8Array): string {
  let binary = '';
  const chunkSize = 0x8000;
  for (let offset = 0; offset < bytes.length; offset += chunkSize) {
    binary += String.fromCharCode(...bytes.subarray(offset, offset + chunkSize));
  }
  return btoa(binary);
}

/**
 * Keeps a torrent dropped through a WebView drag operation in application
 * storage. WebView2 exposes dropped files as bytes instead of an absolute
 * path when native drag/drop handling is disabled for HTML text drops.
 */
export async function stageDroppedTorrent(fileName: string, bytes: Uint8Array): Promise<string> {
  if (!isTauriRuntime()) throw new Error('当前环境不支持接收拖入的种子文件');
  if (bytes.byteLength > 16 * 1024 * 1024) throw new Error('种子文件超过 16 MiB 安全限制');
  return invoke<string>('stage_dropped_torrent', {
    fileName,
    contentsBase64: encodeBase64(bytes),
  });
}

export async function confirmDownload(taskId: string, selectedIds: string[] = [], destinationDirectory?: string, privateDownload = false, postProcessingNote?: string): Promise<QueueTask | undefined> {
  if (!isTauriRuntime()) return undefined;
  return invoke<QueueTask>('confirm_download_command', { taskId, selectedIds, ...(destinationDirectory ? { destinationDirectory } : {}), ...(privateDownload ? { privateDownload } : {}), ...(postProcessingNote?.trim() ? { postProcessingNote } : {}) });
}

export async function submitVideoFormats(video: VideoPreview, ids: string[], destinationDirectory: string | undefined, privateDownload: boolean, onCreated: (task: QueueTask) => void, postProcessingNote?: string) {
  if (!isTauriRuntime()) throw new Error(t('video.error.desktop'));
  const selected = video.formats.filter(format => ids.includes(format.id));
  if (!selected.length) throw new Error(t('video.error.formats'));
  const outcomes = await Promise.allSettled(selected.map(async format => {
    const task = await invoke<QueueTask>('submit_video_download', { previewId: video.previewId, formatId: format.id, destinationDirectory, privateDownload, postProcessingNote });
    onCreated(task);
  }));
  const failures = outcomes.flatMap((result, i) => result.status === 'rejected' ? [{ selectedId: selected[i].id, message: videoMessage(result.reason) }] : []);
  if (!failures.length) await discardPreparedDownload('video-page:' + video.previewId).catch(() => undefined);
  return failures;
}

export async function setTaskPrivacy(taskId: string, isPrivate: boolean): Promise<QueueTask | undefined> {
  if (!isTauriRuntime()) return undefined;
  return invoke<QueueTask>('set_task_private', { taskId, isPrivate });
}

export type PersistedTaskScope = 'active' | 'terminal';

export async function listPersistedTasks(scope?: PersistedTaskScope): Promise<QueueTask[]> {
  return isTauriRuntime() ? invoke<QueueTask[]>('list_tasks', { scope: scope ?? null }) : [];
}

export async function runTaskAction(taskId: string, action: Extract<TaskCommand, 'pause' | 'resume' | 'cancel' | 'retry' | 'retry_cleanup'>): Promise<TaskActionResult> {
  if (!isTauriRuntime()) return {};
  return duplicateResult(await invoke<NativeTaskOutcome>('task_action', { taskId, action }).catch(error => {
    if (typeof error === 'string' && error.startsWith('video.error.')) throw new Error(videoMessage(error));
    throw error;
  }));
}

export async function openTaskDirectory(taskId: string): Promise<void> {
  if (isTauriRuntime()) await invoke('open_task_directory', { taskId });
}

export async function openTaskFile(taskId: string): Promise<void> {
  if (isTauriRuntime()) await invoke('open_task_file', { taskId });
}

export async function confirmCloudShareDownload(planSource: string, selectedIds: string[] = [], destinationDirectory?: string, privateDownload = false, onCreated?: (task: QueueTask) => void, postProcessingNote?: string): Promise<CloudShareBatchResult> {
  if (!isTauriRuntime()) return { outcomes: [], failures: [] };
  const onCreatedChannel = new Channel<QueueTask>();
  onCreatedChannel.onmessage = (task) => onCreated?.(task);
  const result = await invoke<NativeCloudShareBatchResult>('confirm_cloud_share_download', { planSource, selectedIds, ...(destinationDirectory ? { destinationDirectory } : {}), ...(privateDownload ? { privateDownload } : {}), ...(postProcessingNote?.trim() ? { postProcessingNote } : {}), onCreated: onCreatedChannel });
  return { outcomes: result.outcomes.map(duplicateResult), failures: result.failures };
}

export async function playTaskVideos(taskId: string): Promise<void> {
  if (isTauriRuntime()) await invoke('play_task_videos', { taskId });
}

export async function resolveDuplicateDownload(taskId: string, action: 'resume' | 'restart_missing' | 'overwrite'): Promise<QueueTask | undefined> {
  return isTauriRuntime() ? invoke<QueueTask>('resolve_duplicate_download', { taskId, action }) : undefined;
}

export async function moveTaskToRecycle(taskId: string): Promise<QueueTask | undefined> {
  return isTauriRuntime() ? invoke<QueueTask>('recycle_task', { taskId }) : undefined;
}

export async function purgeRecycledTasks(taskIds: string[], deleteFiles: boolean): Promise<void> {
  if (isTauriRuntime()) await invoke('purge_recycled_tasks', { taskIds, deleteFiles });
}

export async function deleteTasks(taskIds: string[], deleteFiles: boolean): Promise<void> {
  if (isTauriRuntime()) await invoke('delete_tasks', { taskIds, deleteFiles });
}

export async function selectDownloadFiles(taskId: string, indices: number[]): Promise<QueueTask | undefined> {
  return isTauriRuntime() ? invoke<QueueTask>('select_files', { taskId, indices }) : undefined;
}

export async function cancelDownloadFile(taskId: string, fileIndex: number): Promise<QueueTask | undefined> {
  return isTauriRuntime() ? invoke<QueueTask>('cancel_download_file', { taskId, fileIndex }) : undefined;
}

export async function getDownloadEngineState(): Promise<DownloadEngineState> {
  return isTauriRuntime()
    ? invoke<DownloadEngineState>('get_download_engine_state')
    : { available: true, retrying: false };
}

export async function retryDownloadEngine(): Promise<DownloadEngineState> {
  return isTauriRuntime()
    ? invoke<DownloadEngineState>('retry_download_engine')
    : { available: true, retrying: false };
}

export async function getDownloadGlobalStat(): Promise<DownloadGlobalStat> {
  return isTauriRuntime()
    ? invoke<DownloadGlobalStat>('get_download_global_stat')
    : { download_speed: 0, upload_speed: 0, num_active: 0 };
}

export const defaultSettings: AppSettings = {
  downloadDirectory: '',
  launcherVisibility: 'always',
  proxy: { enabled: false, protocol: 'http', host: '', port: 7890, username: '', password: '' },
  ai: { enabled: false, baseUrl: '', model: '', apiKeyConfigured: false },
  magnetMetadata: { enabled: true, baseUrl: 'http://peer.aigutta.com:8080' },
  aria2: { rpcPort: 6800, rpcToken: '', maxConcurrentDownloads: 20, maxConnectionsPerServer: 4, btSeedRatio: 1.5, debugLogging: false, overallDownloadLimit: 0, p2pUploadLimit: 1048576 },
  deleteTransferredShareFilesAfterDownload: true,
  privacyDownloadEnabled: false,
  privacyPasswordConfigured: false,
  cloudUploadLimit: 0,
  downloadStatusNotifications: true,
  associateDownloadLinks: true
};

/** Links clicked in browsers that started or reached the app; each call takes them once. */
export async function takeExternalLinks(): Promise<string[]> {
  return isTauriRuntime() ? invoke<string[]>('take_external_links') : [];
}

export async function getSettings(): Promise<AppSettings> {
  return isTauriRuntime() ? invoke<AppSettings>('get_settings') : defaultSettings;
}

export async function updateSettings(settings: AppSettings): Promise<AppSettings> {
  if (!isTauriRuntime()) return settings;
  try {
    return await invoke<AppSettings>('update_settings', { settings });
  } catch (error) {
    if (typeof error === 'string' && error.startsWith('macos.links.')) {
      throw new Error(t(error as TranslationKey));
    }
    throw error;
  }
}

export async function configurePrivacyDownload(enabled: boolean, currentPassword?: string, newPassword?: string): Promise<AppSettings> {
  if (!isTauriRuntime()) return { ...defaultSettings, privacyDownloadEnabled: enabled, privacyPasswordConfigured: Boolean(newPassword) };
  return invoke<AppSettings>('configure_privacy_download', {
    enabled,
    currentPassword: currentPassword || null,
    newPassword: newPassword || null
  });
}

export async function getPrivacyTaskVisibility(): Promise<PrivacyTaskVisibility> {
  return isTauriRuntime() ? invoke<PrivacyTaskVisibility>('get_privacy_task_visibility') : { enabled: false, unlocked: false };
}

export async function unlockPrivacyTasks(password: string): Promise<PrivacyTaskVisibility> {
  return isTauriRuntime() ? invoke<PrivacyTaskVisibility>('unlock_privacy_tasks', { password }) : { enabled: true, unlocked: password.length > 0 };
}

export async function lockPrivacyTasks(): Promise<PrivacyTaskVisibility> {
  return isTauriRuntime() ? invoke<PrivacyTaskVisibility>('lock_privacy_tasks') : { enabled: false, unlocked: false };
}

const appUpdateCacheKey = 'versadown.app-update.v1';
const appUpdateCacheLifetimeMs = 12 * 60 * 60 * 1000;

function readCachedAppUpdate(): AppUpdate | null | undefined {
  try {
    const cached = JSON.parse(window.localStorage.getItem(appUpdateCacheKey) ?? 'null') as { appVersion?: unknown; checkedAt?: unknown; update?: unknown } | null;
    if (!cached || cached.appVersion !== appVersion || typeof cached.checkedAt !== 'number' || Date.now() - cached.checkedAt >= appUpdateCacheLifetimeMs) return undefined;
    if (cached.update === null) return null;
    if (typeof cached.update !== 'object' || cached.update === null) return undefined;
    const update = cached.update as Partial<AppUpdate>;
    return typeof update.version === 'string' && typeof update.notes === 'string' && typeof update.downloadUrl === 'string'
      ? { version: update.version, notes: update.notes, downloadUrl: update.downloadUrl }
      : undefined;
  } catch {
    return undefined;
  }
}

function cacheAppUpdate(update: AppUpdate | null) {
  try {
    window.localStorage.setItem(appUpdateCacheKey, JSON.stringify({ appVersion, checkedAt: Date.now(), update }));
  } catch {
    // Update checks remain functional when browser storage is unavailable.
  }
}

export async function checkForAppUpdate(): Promise<AppUpdate | null> {
  if (!isTauriRuntime()) return null;
  const cached = readCachedAppUpdate();
  if (cached !== undefined) return cached;
  const update = await invoke<AppUpdate | null>('check_for_app_update');
  cacheAppUpdate(update);
  return update;
}

export async function openExternalUrl(url: string): Promise<void> {
  const destination = new URL(url);
  if (destination.protocol !== 'https:' || destination.hostname !== 'versadown.aigutta.com') throw new Error('更新地址无效');
  if (isTauriRuntime()) {
    const { openUrl } = await import('@tauri-apps/plugin-opener');
    await openUrl(destination);
    return;
  }
  window.open(destination, '_blank', 'noopener,noreferrer');
}

export async function updateAiSettings(settings: AiSettingsInput): Promise<AiSettings> {
  if (!isTauriRuntime()) {
    return { enabled: settings.enabled, baseUrl: settings.baseUrl, model: settings.model, apiKeyConfigured: Boolean(settings.apiKey) };
  }
  return invoke<AiSettings>('update_ai_settings', { settings });
}

export async function listAiModels(settings: AiSettingsInput): Promise<string[]> {
  if (!isTauriRuntime()) return [];
  return invoke<string[]>('list_ai_models', { settings });
}
