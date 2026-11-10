import { useEffect, useLayoutEffect, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { DownloadFile, QueueTask, TaskCommand } from '../lib/types';
import { formatBytes, formatEta, formatSpeed, transferProgress } from '../lib/downloadEvents';
import { isPrivateTask } from '../lib/queueTask';
import { reorderTaskIds } from '../lib/taskOrder';
import { t, useTranslation } from '../i18n';
import { xunleiDownloadMessage, xunleiFallbackMessage } from '../i18n/xunleiDownloadMessage';

interface DownloadQueueProps {
  tasks: readonly QueueTask[];
  privacyEnabled: boolean;
  selectedTaskId: string | null;
  selectedTaskIds?: readonly string[];
  onSelect(id: string): void;
  onSelectionChange?(ids: readonly string[]): void;
  onCommand(id: string, command: TaskCommand): void;
  onBatchCommand?(ids: readonly string[], command: BatchTaskCommand): void;
  onReorder?(orderedTaskIds: readonly string[]): void;
  onFileSelection?(taskId: string, fileIndex: number, selected: boolean): void;
  onFileCancel?(taskId: string, fileIndex: number): void;
}

export type BatchTaskCommand = 'resume' | 'pause' | 'delete' | 'copy_link' | 'move_to_private' | 'move_to_normal';

function magnetName(value: string): string {
  try {
    const query = value.slice(value.indexOf('?') + 1);
    const params = new URLSearchParams(query);
    const displayName = params.get('dn')?.trim();
    if (displayName) return displayName;
    const hash = params.get('xt')?.split(':').pop();
    if (hash) return `${t('download.name.magnet')} ${hash.slice(0, 8)}`;
  } catch {
    // Fall through to the stable generic label for malformed legacy data.
  }
  return t('download.name.magnet');
}

export function displayTaskName(task: QueueTask): string {
  const candidate = task.name?.trim();
  if (task.kind === 'magnet' && candidate.toLowerCase().startsWith('magnet:')) return magnetName(candidate);
  return candidate || t('download.name.unnamed');
}

function childPathParts(value: string): string[] {
  const parts = value.trim().replace(/\\/g, '/').replace(/\/{2,}/g, '/').split('/').filter(Boolean);
  return /^[a-z]:$/i.test(parts[0] ?? '') ? parts.slice(1) : parts;
}

function btFilePathPrefixLength(files: readonly DownloadFile[], taskName: string): number {
  const paths = files.map((file) => childPathParts(file.path));
  if (!paths.length) return 0;
  const titlePositions = paths.map((parts) => parts.findIndex((part) => part === taskName));
  if (titlePositions[0] >= 0 && titlePositions.every((position) => position === titlePositions[0])) return titlePositions[0] + 1;
  if (paths.some((parts) => parts.length < 2)) return 0;
  const root = paths[0][0];
  return paths.every((parts) => parts[0] === root) ? 1 : 0;
}

interface BtTreeEntry {
  file: DownloadFile;
  parts: string[];
}

function btFileTask(task: QueueTask, file: DownloadFile, name: string): QueueTask {
  return {
    id: `${task.id}-file-${file.index}`,
    name,
    kind: 'file',
    status: task.status,
    progress: file.length > 0 ? Math.min(100, Math.floor(file.completed_length * 100 / file.length)) : 0,
    total_length: file.length,
    completed_length: file.completed_length,
    file_index: file.index,
    selected: file.selected,
  };
}

function buildBtFileTree(task: QueueTask): readonly QueueTask[] {
  const files = task.files ?? [];
  const prefixLength = btFilePathPrefixLength(files, displayTaskName(task));
  const entries: BtTreeEntry[] = files.map((file) => {
    const original = childPathParts(file.path);
    const parts = original.slice(prefixLength);
    return { file, parts: parts.length ? parts : [original.at(-1) ?? file.path] };
  });
  const build = (current: readonly BtTreeEntry[], parentPath: string[]): QueueTask[] => {
    const directories = new Map<string, BtTreeEntry[]>();
    const files: QueueTask[] = [];
    current.forEach((entry) => {
      if (entry.parts.length <= 1) {
        files.push(btFileTask(task, entry.file, entry.parts[0]));
        return;
      }
      const [directory, ...rest] = entry.parts;
      const children = directories.get(directory) ?? [];
      children.push({ file: entry.file, parts: rest });
      directories.set(directory, children);
    });
    return [
      ...Array.from(directories, ([name, children]) => {
        const descendantFiles = children.map((child) => child.file);
        const totalLength = descendantFiles.reduce((total, file) => total + file.length, 0);
        const completedLength = descendantFiles.reduce((total, file) => total + file.completed_length, 0);
        const path = [...parentPath, name];
        return {
          id: `${task.id}-directory-${path.join('/')}`,
          name,
          kind: 'folder' as const,
          status: task.status,
          progress: totalLength > 0 ? Math.min(100, Math.floor(completedLength * 100 / totalLength)) : 0,
          total_length: totalLength,
          completed_length: completedLength,
          children: build(children, path),
        };
      }),
      ...files,
    ];
  };
  return build(entries, []);
}

const videoExtensions = new Set(['3gp', 'asf', 'avi', 'flv', 'm2ts', 'm4v', 'mkv', 'mov', 'mp4', 'mpeg', 'mpg', 'mts', 'ts', 'webm', 'wmv']);
const audioExtensions = new Set(['aac', 'aiff', 'alac', 'ape', 'flac', 'm4a', 'mka', 'mp3', 'oga', 'ogg', 'opus', 'wav', 'wma']);
const playableMediaExtensions = new Set([...videoExtensions, ...audioExtensions]);
type AssociatedFileType = 'text' | 'image' | 'pdf' | 'office';

const associatedFileExtensions: Record<AssociatedFileType, ReadonlySet<string>> = {
  text: new Set(['cfg', 'conf', 'csv', 'css', 'htm', 'html', 'ini', 'json', 'log', 'md', 'markdown', 'rst', 'text', 'toml', 'tsv', 'txt', 'xml', 'yaml', 'yml']),
  image: new Set(['avif', 'bmp', 'gif', 'heic', 'ico', 'jpeg', 'jpg', 'png', 'svg', 'tif', 'tiff', 'webp']),
  pdf: new Set(['pdf']),
  office: new Set(['doc', 'docm', 'docx', 'dot', 'dotm', 'dotx', 'odp', 'ods', 'odt', 'pot', 'potm', 'potx', 'pps', 'ppsm', 'ppsx', 'ppt', 'pptm', 'pptx', 'rtf', 'wps', 'xls', 'xlsb', 'xlsm', 'xlsx', 'xlt', 'xltm', 'xltx']),
};

const associatedFileLabels: Record<AssociatedFileType, string> = {
  text: 'download.open.text',
  image: 'download.open.image',
  pdf: 'download.open.pdf',
  office: 'download.open.office',
};

function hasPlayableMediaExtension(path: string): boolean {
  const extension = path.trim().split(/[?#]/, 1)[0].split('.').pop()?.toLowerCase();
  return extension !== undefined && playableMediaExtensions.has(extension);
}

function associatedFileType(path: string): AssociatedFileType | undefined {
  const basename = path.trim().replace(/\\/g, '/').split('/').at(-1);
  const extension = basename?.split('.').pop()?.toLowerCase();
  if (!extension) return undefined;
  return (Object.keys(associatedFileExtensions) as AssociatedFileType[]).find((type) => associatedFileExtensions[type].has(extension));
}

function taskAssociatedFileType(task: QueueTask): AssociatedFileType | undefined {
  if (task.kind === 'folder' || task.children?.length) return undefined;
  const files = task.files?.filter((file) => file.selected !== false) ?? [];
  return files.length === 1 ? associatedFileType(files[0].path) : undefined;
}

type CompletedQuickAction = 'image' | 'video' | 'audio' | 'directory';

function completedQuickActionText(action: CompletedQuickAction, name: string) {
  const keys: Record<CompletedQuickAction, { aria: string; title: string }> = {
    image: { aria: 'download.quick.image.aria', title: 'download.quick.image.title' },
    video: { aria: 'download.quick.video.aria', title: 'download.quick.video.title' },
    audio: { aria: 'download.quick.audio.aria', title: 'download.quick.audio.title' },
    directory: { aria: 'download.quick.directory.aria', title: 'download.quick.directory.title' },
  };
  const labels = keys[action];
  return { aria: t(labels.aria as Parameters<typeof t>[0], { name }), title: t(labels.title as Parameters<typeof t>[0]) };
}

function selectedTaskPaths(task: QueueTask): string[] {
  if (task.selected === false) return [];
  if (task.files?.length) return task.files.filter((file) => file.selected !== false).map((file) => file.path);
  if (task.children?.length) return task.children.flatMap(selectedTaskPaths);
  return task.kind === 'folder' ? [] : [task.name];
}

function completedQuickAction(task: QueueTask): CompletedQuickAction {
  const paths = selectedTaskPaths(task);
  if (!paths.length) return 'directory';
  const types = paths.map((path) => {
    const extension = path.trim().replace(/\\/g, '/').split('/').at(-1)?.split('.').pop()?.toLowerCase();
    if (extension && videoExtensions.has(extension)) return 'video';
    if (extension && audioExtensions.has(extension)) return 'audio';
    if (extension && associatedFileExtensions.image.has(extension)) return 'image';
    return 'directory';
  });
  return types[0] !== 'directory' && types.every((type) => type === types[0]) ? types[0] : 'directory';
}

export function completedTaskSize(task: QueueTask): string | undefined {
  const files = task.files?.filter((file) => file.selected !== false) ?? [];
  if (files.length) {
    const downloaded = files.reduce((total, file) => total + (file.completed_length > 0 ? file.completed_length : file.length), 0);
    return downloaded > 0 ? formatBytes(downloaded) : undefined;
  }
  const downloaded = task.completed_length && task.completed_length > 0
    ? task.completed_length
    : task.total_length && task.total_length > 0 ? task.total_length : undefined;
  return downloaded !== undefined ? formatBytes(downloaded) : undefined;
}

export function completedAt(completedAt: number | undefined): string | undefined {
  if (!Number.isFinite(completedAt) || !completedAt || completedAt <= 0) return undefined;
  const date = new Date(completedAt < 1_000_000_000_000 ? completedAt * 1000 : completedAt);
  if (Number.isNaN(date.getTime())) return undefined;
  const pad = (value: number) => String(value).padStart(2, '0');
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(date.getHours())}:${pad(date.getMinutes())}`;
}

function childFileSize(task: QueueTask): string {
  const selectedFiles = task.files?.filter((file) => file.selected !== false);
  const completed = task.completed_length ?? (selectedFiles?.length
    ? selectedFiles.reduce((total, file) => total + file.completed_length, 0) : undefined);
  const knownTotal = task.total_length && task.total_length > 0 ? task.total_length
    : selectedFiles?.reduce((total, file) => total + file.length, 0);
  if (task.status !== 'completed' && completed !== undefined) {
    return `${formatBytes(completed)} / ${knownTotal && knownTotal > 0 ? formatBytes(knownTotal) : t('common.unknown')}`;
  }
  if (typeof task.total_length === 'number' && task.total_length > 0) return formatBytes(task.total_length);
  const fileSize = task.files
    ?.filter((file) => file.selected !== false)
    .reduce((total, file) => total + Math.max(file.length, file.completed_length), 0);
  if (fileSize && fileSize > 0) return formatBytes(fileSize);
  if (task.status === 'completed' && task.completed_length && task.completed_length > 0) return formatBytes(task.completed_length);
  return task.size || '—';
}

const cloudProviderTaskLabels: Record<string, string> = {
  quark: 'cloud.provider.quark',
  pikpak: 'cloud.provider.pikpak',
  baidu: 'cloud.provider.baidu',
  tianyi: 'cloud.provider.tianyi',
  aliyun: 'cloud.provider.aliyun',
  xunlei: 'cloud.provider.xunlei',
  uc: 'cloud.provider.uc',
};

function cloudTaskSubtitle(source: string | undefined): string | undefined {
  const provider = source?.match(/^cloud(?:-share|-directory|-directory-item)?:\/\/([^/]+)/i)?.[1]?.toLowerCase();
  const labelKey = provider && cloudProviderTaskLabels[provider];
  return labelKey ? t('download.cloudTask', { name: t(labelKey as Parameters<typeof t>[0]) }) : undefined;
}

function taskSubtitle(task: QueueTask): string | undefined {
  if (task.error) return xunleiDownloadMessage(task.error);
  const cloudSubtitle = cloudTaskSubtitle(task.source);
  if (task.source?.toLowerCase().startsWith('cloud://xunlei/') && task.statusMessage === '正在使用迅雷svip加速') {
    return t('download.cloudTaskSvip', { name: t('cloud.provider.xunlei') });
  }
  if (task.statusMessage) return xunleiDownloadMessage(task.statusMessage);
  const hasTransferData = task.progress > 0 || (task.completed_length ?? 0) > 0 || (task.total_length ?? 0) > 0;
  if (task.kind === 'magnet') return hasTransferData ? t('download.kind.magnet') : t('download.kind.magnetMetadata');
  if (task.kind === 'torrent') return hasTransferData ? t('download.kind.torrent') : t('download.kind.torrentMetadata');
  if (task.kind === 'ed2k') return t('download.kind.ed2k');
  if (task.kind === 'ftp') return t('download.kind.ftp');
  if (task.kind === 'hls') return t('download.kind.hls');
  if (task.kind === 'dash') return t('download.kind.dash');
  return cloudSubtitle;
}

function isFetchingTaskMetadata(task: QueueTask, subtitle: string | undefined): boolean {
  const metadataPending = !task.error
    && task.status === 'downloading'
    && (task.kind === 'magnet' || task.kind === 'torrent')
    && Boolean(subtitle?.match(/正在获取(?:完整)?(?:文件|种子)信息/));
  return metadataPending || isWaitingForCloudAuthorization(task) || isRecoveringCloudAuthorization(task);
}

function isWaitingForCloudAuthorization(task: QueueTask): boolean {
  return task.status === 'paused'
    && task.failure_code === 'authorization_pending'
    && task.error === '恢复网盘登录后继续下载';
}

function isRecoveringCloudAuthorization(task: QueueTask): boolean {
  return task.status === 'paused'
    && (task.failure_code === 'authorization_checking'
      || task.failure_code === 'authorization_recovering');
}

function taskPeerMetric(task: QueueTask): string | undefined {
  if (task.status !== 'downloading') return undefined;
  const connections = task.connections ?? 0;
  return task.kind === 'magnet' || task.kind === 'torrent' || task.kind === 'ed2k'
    ? `(${connections}/${task.discovered_nodes ?? 0})`
    : undefined;
}

function TaskKindIcon({ kind }: { kind: QueueTask['kind'] }) {
  if (kind === 'folder') return <svg aria-hidden="true" viewBox="0 0 24 24"><path d="M3.5 7.5h6l1.7 2h9.3v8.8a2.2 2.2 0 0 1-2.2 2.2H5.7a2.2 2.2 0 0 1-2.2-2.2z" /><path d="M3.5 7.5V5.7a2.2 2.2 0 0 1 2.2-2.2h4.2l1.7 2h6.7a2.2 2.2 0 0 1 2.2 2.2v1.8" /></svg>;
  if (kind === 'magnet' || kind === 'torrent') return <svg aria-hidden="true" viewBox="0 0 24 24"><path d="M7 4v8a5 5 0 0 0 10 0V4" /><path d="M7 8h4M13 8h4M7 4h4M13 4h4" /></svg>;
  if (kind === 'ed2k') return <svg aria-hidden="true" viewBox="0 0 24 24"><path d="M8.5 8.5 5 12l3.5 3.5M15.5 8.5 19 12l-3.5 3.5" /><path d="M10 17 14 7" /></svg>;
  if (kind === 'hls' || kind === 'dash') return <svg aria-hidden="true" viewBox="0 0 24 24"><circle cx="12" cy="12" r="8.5" /><path d="m10 8 5 4-5 4z" /></svg>;
  return <svg aria-hidden="true" viewBox="0 0 24 24"><path d="M12 3v12" /><path d="m7.5 11 4.5 4.5 4.5-4.5" /><path d="M5 20h14" /></svg>;
}

function ActionIcon({ action }: { action: 'pause' | 'resume' | 'cancel' | 'retry' | 'reauthorize' | 'play' | 'audio' | 'folder' | AssociatedFileType | 'more' }) {
  return <svg aria-hidden="true" viewBox="0 0 24 24">{action === 'pause'
    ? <><path d="M8 5v14" /><path d="M16 5v14" /></>
    : action === 'resume' ? <path d="m9 6 9 6-9 6z" />
      : action === 'cancel' ? <><circle cx="12" cy="12" r="8" /><path d="m9 9 6 6M15 9l-6 6" /></>
        : action === 'retry' || action === 'reauthorize' ? <><path d="M4.5 10a8 8 0 1 1 1.8 6" /><path d="M4.5 5v5h5" /></>
          : action === 'play' ? <><circle cx="12" cy="12" r="8.5" /><path d="m10 8 5 4-5 4z" /></>
            : action === 'audio' ? <><path d="M10 17V5l9-2v12" /><ellipse cx="7" cy="18" rx="3" ry="2" /><ellipse cx="16" cy="16" rx="3" ry="2" /></>
            : action === 'folder' ? <><path d="M3.5 7.5h6l1.7 2h9.3v8.8a2.2 2.2 0 0 1-2.2 2.2H5.7a2.2 2.2 0 0 1-2.2-2.2z" /><path d="M3.5 7.5V5.7a2.2 2.2 0 0 1 2.2-2.2h4.2l1.7 2h6.7" /></>
              : action === 'text' ? <><path d="M7 3.5h7l3 3V20.5H7z" /><path d="M14 3.5v3h3M9.5 11h5M9.5 14h5M9.5 17h3" /></>
                : action === 'image' ? <><rect x="3.5" y="5" width="17" height="14" rx="2" /><circle cx="9" cy="10" r="1.5" /><path d="m5.5 17 4.5-4 3.5 3 2.5-2 2.5 3" /></>
                  : action === 'pdf' ? <><path d="M7 3.5h7l3 3V20.5H7z" /><path d="M14 3.5v3h3M9.5 16.5h5M9.5 12h5" /></>
                    : action === 'office' ? <><path d="M7 3.5h7l3 3V20.5H7z" /><path d="M14 3.5v3h3M9.5 11h5M9.5 15h5M12 9v8" /></>
                      : <><circle cx="5" cy="12" r="1.2" /><circle cx="12" cy="12" r="1.2" /><circle cx="19" cy="12" r="1.2" /></>}</svg>;
}

type TaskMenuCommand = TaskCommand;

interface ContextMenuState { tasks: readonly QueueTask[]; x: number; y: number }

interface TaskContextMenuPositionInput {
  x: number;
  y: number;
  menuWidth: number;
  menuHeight: number;
  viewportWidth: number;
  viewportHeight: number;
}

export function taskContextMenuPosition({ x, y, menuWidth, menuHeight, viewportWidth, viewportHeight }: TaskContextMenuPositionInput) {
  const inset = 8;
  return {
    left: Math.max(inset, Math.min(x, viewportWidth - menuWidth - inset)),
    top: Math.max(inset, Math.min(y, viewportHeight - menuHeight - inset)),
  };
}

function contextCommands(task: QueueTask, privacyEnabled: boolean): Array<{ command: TaskMenuCommand; label: string; danger?: boolean; separated?: boolean }> {
  const commands: Array<{ command: TaskMenuCommand; label: string; danger?: boolean; separated?: boolean }> = [];
  if (task.status === 'downloading') commands.push({ command: 'pause', label: t('download.action.pause') });
  if (task.status === 'paused') commands.push(task.reauthorization_required
    ? { command: 'reauthorize', label: t('download.action.reauthorize') }
    : { command: 'resume', label: t('download.action.resume') });
  if (task.status === 'failed') commands.push(task.reauthorization_required
    ? { command: 'reauthorize', label: t('download.action.reauthorize') }
    : { command: 'retry', label: t('download.action.retry') });
  if (task.status === 'completed' && task.cleanup_pending) commands.push({ command: 'retry_cleanup', label: t('download.action.retryCleanup') });
  commands.push({ command: 'open_directory', label: t('download.action.openDirectory'), separated: commands.length > 0 });
  commands.push({ command: 'details', label: t('download.action.details') });
  if (task.source) commands.push({ command: 'copy_link', label: t('download.action.copyLink') });
  if (privacyEnabled) commands.push(isPrivateTask(task)
    ? { command: 'move_to_normal', label: t('download.action.moveToNormal'), separated: commands.length > 0 }
    : { command: 'move_to_private', label: t('download.action.moveToPrivate'), separated: commands.length > 0 });
  commands.push({ command: 'delete', label: t('download.action.deleteTask'), danger: true, separated: true });
  return commands;
}

function batchContextCommands(tasks: readonly QueueTask[], privacyEnabled: boolean): Array<{ command: TaskMenuCommand; label: string; danger?: boolean; separated?: boolean }> {
  const commands: Array<{ command: TaskMenuCommand; label: string; danger?: boolean; separated?: boolean }> = [];
  if (tasks.some((task) => task.status === 'paused' || task.status === 'failed')) commands.push({ command: 'resume', label: t('download.action.start') });
  if (tasks.some((task) => task.status === 'downloading')) commands.push({ command: 'pause', label: t('download.action.pause') });
  if (tasks.some((task) => Boolean(task.source))) commands.push({ command: 'copy_link', label: t('download.action.copyLink'), separated: commands.length > 0 });
  if (privacyEnabled && tasks.some((task) => !isPrivateTask(task))) commands.push({ command: 'move_to_private', label: t('download.action.moveToPrivate'), separated: commands.length > 0 });
  if (privacyEnabled && tasks.some(isPrivateTask)) commands.push({ command: 'move_to_normal', label: t('download.action.moveToNormal'), separated: commands.length > 0 });
  commands.push({ command: 'delete', label: t('download.action.delete'), danger: true, separated: true });
  return commands;
}

function MenuIcon({ command }: { command: TaskMenuCommand }) {
  const deletePath = <><path d="M5 7h14M9 7V4h6v3M8 10v7M12 10v7M16 10v7M6.5 7l.8 13h9.4l.8-13" /></>;
  const paths: Record<TaskMenuCommand, React.ReactNode> = {
    pause: <><path d="M8 5v14" /><path d="M16 5v14" /></>, resume: <path d="m9 6 9 6-9 6z" />,
    cancel: <><circle cx="12" cy="12" r="8" /><path d="m9 9 6 6M15 9l-6 6" /></>, retry: <><path d="M4.5 10a8 8 0 1 1 1.8 6" /><path d="M4.5 5v5h5" /></>,
    reauthorize: <><path d="M4.5 10a8 8 0 1 1 1.8 6" /><path d="M4.5 5v5h5" /></>,
    retry_cleanup: <><path d="M4.5 10a8 8 0 1 1 1.8 6" /><path d="M4.5 5v5h5" /></>,
    open_directory: <><path d="M3.5 7.5h6l1.7 2h9.3v8.8a2.2 2.2 0 0 1-2.2 2.2H5.7a2.2 2.2 0 0 1-2.2-2.2z" /><path d="M3.5 7.5V5.7a2.2 2.2 0 0 1 2.2-2.2h4.2l1.7 2h6.7" /></>,
    open_file: <><path d="M7 3.5h7l3 3V20.5H7z" /><path d="M14 3.5v3h3M9.5 12h5M9.5 16h5" /></>,
    play_videos: <><circle cx="12" cy="12" r="8.5" /><path d="m10 8 5 4-5 4z" /></>,
    copy_link: <><rect x="8" y="8" width="11" height="11" rx="2" /><path d="M16 8V6a2 2 0 0 0-2-2H6a2 2 0 0 0-2 2v8a2 2 0 0 0 2 2h2" /></>,
    details: <><circle cx="12" cy="12" r="8" /><path d="M12 11v5M12 8h.01" /></>,
    move_to_private: <><path d="M6 11V7.5A2.5 2.5 0 0 1 8.5 5h7A2.5 2.5 0 0 1 18 7.5V11" /><rect x="4" y="11" width="16" height="9" rx="1.5" /><path d="m9 15 3 3 3-3M12 12v6" /></>,
    move_to_normal: <><path d="M6 11V7.5A2.5 2.5 0 0 1 8.5 5h7A2.5 2.5 0 0 1 18 7.5V11" /><rect x="4" y="11" width="16" height="9" rx="1.5" /><path d="m15 16-3-3-3 3M12 19v-6" /></>,
    delete: deletePath,
  };
  return <svg aria-hidden="true" viewBox="0 0 24 24">{paths[command]}</svg>;
}

function TaskContextMenu({ state, privacyEnabled, onClose, onCommand }: { state: ContextMenuState; privacyEnabled: boolean; onClose(): void; onCommand(command: TaskMenuCommand): void }) {
  const { t: translate } = useTranslation();
  const menuRef = useRef<HTMLDivElement>(null);
  const isBatch = state.tasks.length > 1;
  const commands = isBatch ? batchContextCommands(state.tasks, privacyEnabled) : contextCommands(state.tasks[0], privacyEnabled);
  const menuTitle = isBatch ? translate('download.selection.count', { count: state.tasks.length }) : displayTaskName(state.tasks[0]);
  const [menuSize, setMenuSize] = useState({ width: 224, height: commands.length * 38 + (isBatch ? 64 : 14) });
  const { left, top } = taskContextMenuPosition({
    x: state.x,
    y: state.y,
    menuWidth: menuSize.width,
    menuHeight: menuSize.height,
    viewportWidth: window.innerWidth,
    viewportHeight: window.innerHeight,
  });

  useLayoutEffect(() => {
    const rect = menuRef.current?.getBoundingClientRect();
    if (!rect?.width || !rect.height) return;
    setMenuSize((current) => current.width === rect.width && current.height === rect.height
      ? current
      : { width: rect.width, height: rect.height });
  }, [state.x, state.y, commands.length]);

  useEffect(() => {
    menuRef.current?.querySelector<HTMLButtonElement>('[role="menuitem"]')?.focus();
    const closeForPointer = (event: PointerEvent) => { if (!menuRef.current?.contains(event.target as Node)) onClose(); };
    const closeForEscape = (event: KeyboardEvent) => { if (event.key === 'Escape') onClose(); };
    const close = () => onClose();
    document.addEventListener('pointerdown', closeForPointer);
    document.addEventListener('keydown', closeForEscape);
    window.addEventListener('blur', close);
    window.addEventListener('resize', close);
    window.addEventListener('scroll', close, true);
    return () => {
      document.removeEventListener('pointerdown', closeForPointer);
      document.removeEventListener('keydown', closeForEscape);
      window.removeEventListener('blur', close);
      window.removeEventListener('resize', close);
      window.removeEventListener('scroll', close, true);
    };
  }, [onClose]);

  function moveFocus(event: React.KeyboardEvent<HTMLDivElement>) {
    if (!['ArrowDown', 'ArrowUp', 'Home', 'End'].includes(event.key)) return;
    event.preventDefault();
    const items = Array.from(menuRef.current?.querySelectorAll<HTMLButtonElement>('[role="menuitem"]') ?? []);
    if (!items.length) return;
    const current = items.indexOf(document.activeElement as HTMLButtonElement);
    const next = event.key === 'Home' ? 0 : event.key === 'End' ? items.length - 1 : event.key === 'ArrowDown' ? (current + 1) % items.length : (current - 1 + items.length) % items.length;
    items[next].focus();
  }

  return createPortal(<div ref={menuRef} className="task-context-menu" role="menu" aria-label={translate('download.taskMenu', { name: menuTitle })} style={{ left, top }} onKeyDown={moveFocus}>
    {isBatch && <header><b>{menuTitle}</b></header>}
    {commands.map((item) => <div key={item.command} className={item.separated ? 'menu-separated' : undefined}><button role="menuitem" className={item.danger ? 'danger' : undefined} onClick={() => onCommand(item.command)}><MenuIcon command={item.command} /><span>{item.label}</span></button></div>)}
  </div>, document.body);
}

interface QueueTreeRowsProps {
  nodes: readonly QueueTask[];
  depth: number;
  expanded: Record<string, boolean>;
  onToggle(id: string): void;
  onCommand(id: string, command: TaskCommand): void;
  onFileSelection?: DownloadQueueProps['onFileSelection'];
  onFileCancel?: DownloadQueueProps['onFileCancel'];
  parentTaskId: string;
  isBtFileTree: boolean;
  completedRoot: boolean;
}

function QueueTreeRows({ nodes, depth, expanded, onToggle, onCommand, onFileSelection, onFileCancel, parentTaskId, isBtFileTree, completedRoot }: QueueTreeRowsProps) {
  return <>{nodes.map((node) => {
    const nodeChildren = node.children ?? [];
    const hasChildren = nodeChildren.length > 0;
    const isFolder = node.kind === 'folder';
    const isExpanded = expanded[node.id] ?? false;
    const nodeName = displayTaskName(node);
    const fileIsSelected = node.selected !== false;
    const selectionAction = node.status === 'paused' || !fileIsSelected ? 'resume' as const : 'pause' as const;
    const primaryAction = node.reauthorization_required ? 'reauthorize' as const
      : node.status === 'downloading' ? 'pause' as const
      : node.status === 'paused' ? 'resume' as const
        : node.status === 'failed' ? (node.reauthorization_required ? 'reauthorize' as const : 'retry' as const) : null;
    const isBtFile = isBtFileTree && node.file_index !== undefined;
    const canUseFileSelection = isBtFile && Boolean(onFileSelection) && ['downloading', 'paused', 'queued'].includes(node.status);
    const canUseTaskCommand = !isBtFile && !isFolder && Boolean(primaryAction);
    const canPlay = !isFolder && node.status === 'completed' && hasPlayableMediaExtension(node.name);
    const openableFileType = node.status === 'completed' ? taskAssociatedFileType(node) : undefined;
    const failure = node.error ?? (node.status === 'failed' ? node.statusMessage : undefined);
    const error = node.status !== 'completed' && failure && xunleiDownloadMessage(failure);
    const fallback = !isFolder && xunleiFallbackMessage(node.statusMessage);
    const detailText = error || fallback;
    const transferSize = childFileSize(node);
    const childSpeed = node.status === 'downloading'
      ? node.speed ?? (node.download_speed !== undefined
        ? formatSpeed(node.download_speed) : undefined)
      : undefined;
    const isFileLeaf = !hasChildren && !isFolder;
    const completionTime = completedRoot && isFileLeaf && node.status === 'completed' ? completedAt(node.completed_at) : undefined;
    const showsCompletionSummary = completedRoot && isFileLeaf && node.status === 'completed';
    return <div className="queue-tree-node" key={node.id}>
      <article className={`child-row queue-tree-row${hasChildren ? ' has-children' : ''}`} style={{ paddingLeft: `${21 + depth * 22}px` }}>
        <span className="child-name">
          {hasChildren && <button className="queue-tree-expand" aria-label={t(isExpanded ? 'common.collapse' : 'common.expand', { name: nodeName })} onClick={() => onToggle(node.id)}><svg aria-hidden="true" viewBox="0 0 20 20"><path d="m7 4 6 6-6 6" /></svg></button>}
          <b title={nodeName}>{nodeName}</b>
          {detailText && <small className={error ? 'queue-tree-error' : 'queue-tree-note'} title={detailText}>{node.status === 'completed' ? t('download.xunlei.completedFallback') : detailText}</small>}
        </span>
        {showsCompletionSummary
          ? <small className="child-completion-time" style={{ gridColumn: 'span 2' }}>{completionTime}</small>
          : <><span className="child-progress"><i aria-hidden="true"><b style={{ width: `${node.progress}%` }} /></i>{childSpeed !== undefined && <span className="child-speed" aria-label={t('download.childSpeed', { name: nodeName })}>{childSpeed}</span>}</span><small className="child-progress-value">{node.progress}%</small></>}
        <small className="child-transfer-size" title={transferSize}>{transferSize}</small>
        {(canUseFileSelection || canUseTaskCommand || canPlay || openableFileType) && <span className="child-actions">
          {canPlay ? <button aria-label={t('download.playNamed', { name: nodeName })} title={t('download.action.play')} onClick={(event) => { event.stopPropagation(); onCommand(node.id, 'play_videos'); }}><ActionIcon action="play" /></button> : <>
            {openableFileType ? <button aria-label={`${t(associatedFileLabels[openableFileType] as Parameters<typeof t>[0])} ${nodeName}`} title={t('download.openDefaultApp')} onClick={(event) => { event.stopPropagation(); onCommand(node.id, 'open_file'); }}><ActionIcon action={openableFileType} /></button> : <button aria-label={`${primaryAction === 'reauthorize' ? t('download.action.reauthorize') : primaryAction === 'retry' ? t('download.action.retry') : selectionAction === 'pause' ? t('download.action.pause') : t('download.action.resume')} ${nodeName}`} title={primaryAction === 'reauthorize' ? t('download.action.reauthorize') : primaryAction === 'retry' ? t('download.action.retry') : t(selectionAction === 'pause' ? 'download.cancelFile' : 'download.resumeFile')} onClick={(event) => {
              event.stopPropagation();
              if (isBtFile) onFileSelection!(parentTaskId, node.file_index!, selectionAction === 'resume');
              else if (primaryAction) onCommand(node.id, primaryAction);
            }}><ActionIcon action={primaryAction === 'reauthorize' ? 'reauthorize' : primaryAction === 'retry' ? 'retry' : selectionAction} /></button>
            }
            {isBtFile && fileIsSelected && onFileCancel
              ? <button className="child-cancel" aria-label={t('download.cancelNamed', { name: nodeName })} title={t('download.cancelFile')} onClick={(event) => { event.stopPropagation(); onFileCancel(parentTaskId, node.file_index!); }}><ActionIcon action="cancel" /></button>
              : !isBtFile && canUseTaskCommand && node.status !== 'failed'
                ? <button className="child-cancel" aria-label={t('download.cancelNamed', { name: nodeName })} title={t('download.cancelFile')} onClick={(event) => { event.stopPropagation(); onCommand(node.id, 'cancel'); }}><ActionIcon action="cancel" /></button>
                : null}
          </>}
        </span>}
      </article>
      {hasChildren && isExpanded && <div className="queue-tree-children"><QueueTreeRows nodes={nodeChildren} depth={depth + 1} expanded={expanded} onToggle={onToggle} onCommand={onCommand} onFileSelection={onFileSelection} onFileCancel={onFileCancel} parentTaskId={parentTaskId} isBtFileTree={isBtFileTree} completedRoot={completedRoot} /></div>}
    </div>;
  })}</>;
}

export function DownloadQueue({ tasks, privacyEnabled, selectedTaskId, selectedTaskIds: controlledSelectedTaskIds, onSelect, onSelectionChange, onCommand, onBatchCommand, onReorder, onFileSelection, onFileCancel }: DownloadQueueProps) {
  const { t: translate } = useTranslation();
  const [expanded, setExpanded] = useState<Record<string, boolean>>({});
  const [localSelectedTaskIds, setLocalSelectedTaskIds] = useState<string[]>(() => selectedTaskId ? [selectedTaskId] : []);
  const selectedTaskIds = controlledSelectedTaskIds ?? localSelectedTaskIds;
  const selectionAnchor = useRef<string | null>(selectedTaskId);
  const heldSelectionModifiers = useRef({ control: false, meta: false, shift: false });
  const [contextMenu, setContextMenu] = useState<ContextMenuState | null>(null);
  const [dragState, setDragState] = useState<{ movingIds: string[]; targetId?: string; placeAfter?: boolean } | null>(null);
  const dragStateRef = useRef<typeof dragState>(null);
  const [previewOrder, setPreviewOrder] = useState<string[] | null>(null);
  const pendingDrag = useRef<{
    pointerId: number;
    originX: number;
    originY: number;
    offsetX: number;
    offsetY: number;
    movingIds: string[];
    order: string[];
    previewOrder: string[] | null;
    preview: HTMLElement | null;
    source: HTMLElement;
  } | null>(null);
  const suppressClickAfterDrag = useRef(false);
  const closeContextMenu = () => setContextMenu(null);
  function updateSelectedTaskIds(next: string[]) {
    setLocalSelectedTaskIds(next);
    onSelectionChange?.(next);
  }
  useEffect(() => {
    if (controlledSelectedTaskIds !== undefined) return;
    const visibleIds = new Set(tasks.map((task) => task.id));
    setLocalSelectedTaskIds((current) => {
      const visibleSelection = current.filter((id) => visibleIds.has(id));
      if (visibleSelection.length) return visibleSelection.length === current.length ? current : visibleSelection;
      if (selectedTaskId && visibleIds.has(selectedTaskId)) return [selectedTaskId];
      return current.length ? [] : current;
    });
  }, [controlledSelectedTaskIds, selectedTaskId, tasks]);
  useEffect(() => {
    const updateHeldModifier = (event: KeyboardEvent, pressed: boolean) => {
      if (event.key === 'Control') heldSelectionModifiers.current.control = pressed;
      else if (event.key === 'Meta') heldSelectionModifiers.current.meta = pressed;
      else if (event.key === 'Shift') heldSelectionModifiers.current.shift = pressed;
    };
    const clearHeldModifiers = () => { heldSelectionModifiers.current = { control: false, meta: false, shift: false }; };
    const keyDown = (event: KeyboardEvent) => updateHeldModifier(event, true);
    const keyUp = (event: KeyboardEvent) => updateHeldModifier(event, false);
    document.addEventListener('keydown', keyDown);
    document.addEventListener('keyup', keyUp);
    window.addEventListener('blur', clearHeldModifiers);
    return () => {
      document.removeEventListener('keydown', keyDown);
      document.removeEventListener('keyup', keyUp);
      window.removeEventListener('blur', clearHeldModifiers);
    };
  }, []);
  useEffect(() => {
    const selectAllTasks = (event: KeyboardEvent) => {
      if (event.defaultPrevented || !(event.ctrlKey || event.metaKey) || event.altKey || event.shiftKey
        || event.key.toLowerCase() !== 'a' || !tasks.length) return;
      const target = event.target instanceof Element ? event.target : document.activeElement;
      if (target?.closest('input, textarea, select, [contenteditable]:not([contenteditable="false"])')) return;
      const modalOpen = [...document.querySelectorAll('[role="dialog"], [role="alertdialog"], [aria-modal="true"]')]
        .some(dialog => !dialog.closest('[hidden], [aria-hidden="true"]'));
      if (modalOpen) return;
      event.preventDefault();
      updateSelectedTaskIds(tasks.map(task => task.id));
      selectionAnchor.current = tasks[0].id;
      setContextMenu(null);
      onSelect(tasks[0].id);
    };
    document.addEventListener('keydown', selectAllTasks);
    return () => document.removeEventListener('keydown', selectAllTasks);
  }, [tasks, onSelect, onSelectionChange]);
  useEffect(() => () => {
    pendingDrag.current?.preview?.remove();
  }, []);
  function runCommand(command: TaskMenuCommand) {
    const targets = contextMenu?.tasks ?? [];
    closeContextMenu();
    if (targets.length > 1) {
      onBatchCommand?.(targets.map((task) => task.id), command as BatchTaskCommand);
      return;
    }
    if (targets[0]) onCommand(targets[0].id, command);
  }
  function selectionModifiers(event: { ctrlKey: boolean; metaKey: boolean; shiftKey: boolean }) {
    const held = heldSelectionModifiers.current;
    return {
      controlOrMeta: event.ctrlKey || event.metaKey || held.control || held.meta,
      shift: event.shiftKey || held.shift,
    };
  }
  function selectTask(event: React.MouseEvent<HTMLElement>, task: QueueTask) {
    if (suppressClickAfterDrag.current) {
      suppressClickAfterDrag.current = false;
      return;
    }
    const currentIndex = tasks.findIndex((item) => item.id === task.id);
    const modifiers = selectionModifiers(event);
    let next: string[];
    if (modifiers.shift && selectionAnchor.current) {
      const anchorIndex = tasks.findIndex((item) => item.id === selectionAnchor.current);
      const start = Math.min(anchorIndex < 0 ? currentIndex : anchorIndex, currentIndex);
      const end = Math.max(anchorIndex < 0 ? currentIndex : anchorIndex, currentIndex);
      next = tasks.slice(start, end + 1).map((item) => item.id);
    } else if (modifiers.controlOrMeta) {
      next = selectedTaskIds.includes(task.id)
        ? selectedTaskIds.filter((id) => id !== task.id)
        : [...selectedTaskIds, task.id];
      if (!next.length) next = [task.id];
      // Keep the first task of a multi-selection as the range anchor. A
      // subsequent Shift-click should extend that original selection instead
      // of discarding tasks selected before the last Ctrl-click.
      if (!selectionAnchor.current || !tasks.some((item) => item.id === selectionAnchor.current)) {
        selectionAnchor.current = task.id;
      }
    } else {
      next = [task.id];
      selectionAnchor.current = task.id;
    }
    updateSelectedTaskIds(next);
    onSelect(next.at(-1) ?? task.id);
  }
  function openContextMenu(task: QueueTask, x: number, y: number) {
    const keepsSelection = selectedTaskIds.includes(task.id);
    const targets = keepsSelection ? tasks.filter((item) => selectedTaskIds.includes(item.id)) : [task];
    if (!keepsSelection) {
      updateSelectedTaskIds([task.id]);
      selectionAnchor.current = task.id;
      onSelect(task.id);
    }
    setContextMenu({ tasks: targets, x, y });
  }
  function beginTaskDrag(event: React.PointerEvent<HTMLElement>, task: QueueTask) {
    // Modifier-clicks are selection gestures, not drag gestures. In
    // particular, suppressing the click after a drag preview would otherwise
    // make Ctrl/Shift selection appear to do nothing.
    const modifiers = selectionModifiers(event);
    if (!onReorder || event.button !== 0 || modifiers.controlOrMeta || modifiers.shift || (event.target as Element).closest('button, input, textarea, select, a')) return;
    const movingIds = selectedTaskIds.includes(task.id)
      ? tasks.filter((item) => selectedTaskIds.includes(item.id)).map((item) => item.id)
      : [task.id];
    const group = event.currentTarget.closest<HTMLElement>('.task-group');
    if (!group) return;
    const bounds = group.getBoundingClientRect();
    pendingDrag.current = {
      pointerId: event.pointerId,
      originX: event.clientX,
      originY: event.clientY,
      offsetX: event.clientX - bounds.left,
      offsetY: event.clientY - bounds.top,
      movingIds,
      order: tasks.map((item) => item.id),
      previewOrder: null,
      preview: null,
      source: event.currentTarget,
    };
    event.currentTarget.setPointerCapture?.(event.pointerId);
  }
  function startTaskDragPreview(pending: NonNullable<typeof pendingDrag.current>) {
    if (pending.preview) return;
    const group = pending.source.closest<HTMLElement>('.task-group');
    if (!group) return;
    const bounds = group.getBoundingClientRect();
    const preview = group.cloneNode(true) as HTMLElement;
    preview.classList.add('task-drag-preview');
    preview.setAttribute('aria-hidden', 'true');
    preview.style.width = `${bounds.width}px`;
    preview.style.height = `${bounds.height}px`;
    if (pending.movingIds.length > 1) {
      const count = document.createElement('span');
      count.className = 'task-drag-preview-count';
      count.textContent = translate('download.selection.count', { count: pending.movingIds.length });
      preview.append(count);
    }
    document.body.append(preview);
    pending.preview = preview;
    const state = { movingIds: pending.movingIds };
    dragStateRef.current = state;
    setDragState(state);
  }
  function updateTaskDrop(event: React.PointerEvent<HTMLElement>) {
    const pending = pendingDrag.current;
    if (!pending || pending.pointerId !== event.pointerId) return;
    const distance = Math.hypot(event.clientX - pending.originX, event.clientY - pending.originY);
    if (!pending.preview && distance < 5) return;
    startTaskDragPreview(pending);
    if (!pending.preview) return;
    pending.preview.style.left = `${event.clientX - pending.offsetX}px`;
    pending.preview.style.top = `${event.clientY - pending.offsetY}px`;
    const target = document.elementFromPoint(event.clientX, event.clientY)?.closest<HTMLElement>('.task-row[data-task-id]');
    const targetId = target?.dataset.taskId;
    if (!targetId || pending.movingIds.includes(targetId)) return;
    event.preventDefault();
    const bounds = target.getBoundingClientRect();
    const next = { movingIds: pending.movingIds, targetId, placeAfter: event.clientY >= bounds.top + bounds.height / 2 };
    dragStateRef.current = next;
    setDragState(next);
    const nextOrder = reorderTaskIds(pending.order, pending.movingIds, targetId, next.placeAfter);
    if (nextOrder.every((id, index) => id === pending.previewOrder?.[index])) return;
    pending.previewOrder = nextOrder;
    setPreviewOrder(nextOrder);
  }
  function clearTaskDrag(pending: NonNullable<typeof pendingDrag.current>) {
    pending.source.releasePointerCapture?.(pending.pointerId);
    pending.preview?.remove();
    pendingDrag.current = null;
    dragStateRef.current = null;
    setDragState(null);
    setPreviewOrder(null);
  }
  function finishTaskDrop(event: React.PointerEvent<HTMLElement>) {
    const pending = pendingDrag.current;
    if (!pending || pending.pointerId !== event.pointerId) return;
    updateTaskDrop(event);
    const nextOrder = pending.previewOrder;
    const didDrag = Boolean(pending.preview);
    clearTaskDrag(pending);
    if (!didDrag) return;
    suppressClickAfterDrag.current = true;
    if (nextOrder) onReorder?.(nextOrder);
  }
  function cancelTaskDrag(event: React.PointerEvent<HTMLElement>) {
    const pending = pendingDrag.current;
    if (!pending || pending.pointerId !== event.pointerId) return;
    clearTaskDrag(pending);
  }
  const taskById = new Map(tasks.map((task) => [task.id, task]));
  const displayedTasks = previewOrder
    ? previewOrder.map((id) => taskById.get(id)).filter((task): task is QueueTask => task !== undefined)
    : tasks;
  return (
    <section className="download-queue" aria-label={translate('download.queue')} onPointerMove={updateTaskDrop} onPointerUp={finishTaskDrop} onPointerCancel={cancelTaskDrag} onContextMenu={(event) => { if (event.target === event.currentTarget) event.preventDefault(); }}>
      <div className="queue-heading"><span>{translate('download.column.task')}</span><span>{translate('download.column.progress')}</span><span>{translate('download.column.status')}</span></div>
      {tasks.length === 0 ? <p className="queue-empty">{translate('download.empty')}</p> : displayedTasks.map((task) => {
        const isExpanded = expanded[task.id] ?? false;
        const isCompleted = task.status === 'completed';
        const completedFallback = isCompleted && task.kind !== 'folder' ? xunleiFallbackMessage(task.statusMessage) : undefined;
        const hasPersistentChildren = Boolean(task.children?.length);
        const isBtFileTree = !hasPersistentChildren && (task.kind === 'magnet' || task.kind === 'torrent') && Boolean(task.files?.length);
        const taskChildren = hasPersistentChildren ? task.children : isBtFileTree ? buildBtFileTree(task) : undefined;
        const hasChildren = Boolean(taskChildren?.length)
          && (task.kind === 'folder' || taskChildren!.length > 1 || taskChildren!.some((child) => child.kind === 'folder'))
          && (!isCompleted || (task.kind === 'folder' && hasPersistentChildren) || isBtFileTree);
        const taskName = displayTaskName(task);
        const isPaused = task.status === 'paused';
        const isDownloading = task.status === 'downloading';
        const speed = !isPaused ? task.speed ?? formatSpeed(task.download_speed) : undefined;
        const eta = isDownloading ? task.eta ?? (task.eta_seconds !== undefined ? formatEta(task.eta_seconds) : undefined) : undefined;
        const hasSpeed = !isPaused && (task.speed !== undefined || task.download_speed !== undefined);
        const transferSize = task.total_length !== undefined || task.completed_length !== undefined ? transferProgress(task) : task.size;
        const subtitle = taskSubtitle(task);
        const fetchingMetadata = isFetchingTaskMetadata(task, subtitle);
        const waitingForCloudAuthorization = isWaitingForCloudAuthorization(task);
        const recoveringCloudAuthorization = isRecoveringCloudAuthorization(task);
        const peerMetric = taskPeerMetric(task);
        const primaryAction = task.reauthorization_required ? 'reauthorize' as const
          : task.status === 'downloading' ? 'pause' as const
          : task.status === 'paused' ? 'resume' as const
          : task.status === 'failed' ? (task.reauthorization_required ? 'reauthorize' as const : 'retry' as const) : null;
        const primaryLabel = primaryAction === 'pause' ? translate('download.action.pause') : primaryAction === 'resume' ? translate('download.action.resume') : primaryAction === 'reauthorize' ? translate('download.action.reauthorize') : translate('download.action.retry');
        const completedSize = isCompleted ? completedTaskSize(task) : undefined;
        const completionTime = isCompleted ? completedAt(task.completed_at) : undefined;
        const completedAction = isCompleted ? completedQuickAction(task) : null;
        const completedActionText = completedAction ? completedQuickActionText(completedAction, taskName) : null;
        const isDragged = dragState?.movingIds.includes(task.id) ?? false;
        const dropPosition = dragState?.targetId === task.id ? (dragState.placeAfter ? ' drop-after' : ' drop-before') : '';
        return <div data-task-id={task.id} className={`task-group${isCompleted ? ' completed-task-group' : ''}${isExpanded ? ' is-expanded' : ''}${isDragged ? ' is-dragging' : ''}${dropPosition}`} key={task.id}>
          <article data-task-id={task.id} className={`task-row ${isCompleted ? 'completed-task' : ''} ${hasChildren ? 'has-children' : ''} ${selectedTaskIds.includes(task.id) ? 'selected' : ''}`} aria-selected={selectedTaskIds.includes(task.id)} onPointerDown={(event) => beginTaskDrag(event, task)} onClick={(event) => selectTask(event, task)} onContextMenu={(event) => { event.preventDefault(); event.stopPropagation(); openContextMenu(task, event.clientX, event.clientY); }}>
            <div className="task-name">
              <span className="expand-slot">{hasChildren && <button className="expand-button" aria-label={t(isExpanded ? 'common.collapse' : 'common.expand', { name: taskName })} onClick={(event) => { event.stopPropagation(); setExpanded((current) => ({ ...current, [task.id]: !isExpanded })); }}><svg aria-hidden="true" viewBox="0 0 20 20"><path d="m7 4 6 6-6 6" /></svg></button>}</span>
              <span className={`kind-icon ${task.kind}`}><TaskKindIcon kind={task.kind} /></span>
              <span className="task-copy"><b title={taskName}>{taskName}</b>{eta && <span className="task-title-eta">{t('download.remaining', { eta })}</span>}</span>
            </div>
            <div className="task-progress"><div className="progress-track" role="progressbar" aria-label={t('download.progressNamed', { name: taskName })} aria-valuemin={0} aria-valuemax={100} aria-valuenow={isCompleted ? 100 : task.progress}><i style={{ width: `${isCompleted ? 100 : task.progress}%` }} /></div><div className="transfer-metrics">{isCompleted ? <>{completedSize && <strong>{completedSize}</strong>}{completionTime && <span>{completionTime}</span>}{task.p2p_sharing && <span>{t('download.p2pSharing')}</span>}<span className="transfer-live"><span className="transfer-paused transfer-completed" title={completedFallback}>{t(completedFallback ? 'download.xunlei.completedFallback' : 'download.status.completed')}</span></span></> : <>{transferSize && <strong>{transferSize}</strong>}{subtitle && <span className={`task-subtitle${task.error && !waitingForCloudAuthorization ? ' task-error' : ''}`} title={subtitle}>{subtitle}{peerMetric && <span className="task-peer-metric">{peerMetric}</span>}{fetchingMetadata && <i className="task-metadata-indicator" aria-hidden="true" />}</span>}<span className="transfer-live">{isPaused ? <span className="transfer-paused">{recoveringCloudAuthorization ? t('download.status.recoveringLogin') : t('download.status.paused')}</span> : <>{hasSpeed && <span className="transfer-speed">{speed}</span>}</>}</span></>}</div></div>
            <div className="task-actions">
              {isCompleted && task.cleanup_pending && <button className="completed-quick-action" aria-label={`${t('download.action.retryCleanup')} ${taskName}`} title={t('download.action.retryCleanup')} onClick={(event) => { event.stopPropagation(); onCommand(task.id, 'retry_cleanup'); }}><ActionIcon action="retry" /></button>}
              {completedAction && completedActionText && <button className="completed-quick-action" aria-label={completedActionText.aria} title={completedActionText.title} onClick={(event) => { event.stopPropagation(); onCommand(task.id, completedAction === 'image' ? 'open_file' : completedAction === 'directory' ? 'open_directory' : 'play_videos'); }}><ActionIcon action={completedAction === 'video' ? 'play' : completedAction === 'directory' ? 'folder' : completedAction} /></button>}
              {task.status === 'queued'
                ? <button className="queued-action" aria-label={`${taskName} ${t('download.status.queued')}`} aria-haspopup="menu" title={t('download.moreActions')} onClick={(event) => { event.stopPropagation(); const rect = event.currentTarget.getBoundingClientRect(); openContextMenu(task, rect.left, rect.bottom + 6); }}>{t('download.status.queued')}</button>
                : primaryAction && <button className={primaryAction === 'reauthorize' ? 'reauthorize-action' : undefined} aria-label={`${primaryLabel} ${taskName}`} title={primaryLabel} onClick={(event) => { event.stopPropagation(); onCommand(task.id, primaryAction); }}><ActionIcon action={primaryAction} />{primaryAction === 'reauthorize' && <span>{t('download.action.reauthorize')}</span>}</button>}
              {task.status !== 'queued' && <button className="task-more-button" aria-label={`${t('download.moreActions')} ${taskName}`} title={t('download.moreActions')} onClick={(event) => { event.stopPropagation(); const rect = event.currentTarget.getBoundingClientRect(); openContextMenu(task, rect.left, rect.bottom + 6); }}><ActionIcon action="more" /></button>}
            </div>
          </article>
          {hasChildren && isExpanded && <div className="children queue-tree"><QueueTreeRows nodes={taskChildren!} depth={0} expanded={expanded} onToggle={(id) => setExpanded((current) => ({ ...current, [id]: !current[id] }))} onCommand={onCommand} onFileSelection={onFileSelection} onFileCancel={onFileCancel} parentTaskId={task.id} isBtFileTree={isBtFileTree} completedRoot={isCompleted} /></div>}
        </div>;
      })}
      {contextMenu && <TaskContextMenu state={contextMenu} privacyEnabled={privacyEnabled} onClose={closeContextMenu} onCommand={runCommand} />}
    </section>
  );
}
