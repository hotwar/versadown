import { videoMessage } from './webVideo';
import type { QueueTask, TaskStatus } from './types';
import { t } from '../i18n';
import { xunleiDownloadMessage } from '../i18n/xunleiDownloadMessage';
import { isPrivateTask } from './queueTask';

let permissionPromise: Promise<boolean> | undefined;

export interface DownloadNotification {
  title: string;
  body: string;
}

function isTerminalNotificationStatus(status: TaskStatus): status is 'completed' | 'failed' {
  return status === 'completed' || status === 'failed';
}

export function notificationForTask(task: QueueTask): DownloadNotification | null {
  if (!isTerminalNotificationStatus(task.status) || isPrivateTask(task) || task.source?.startsWith('cloud-directory-item://')) {
    return null;
  }
  const name = task.name.trim() || '下载任务';
  if (task.status === 'completed') return { title: t('download.status.completed'), body: name };
  const error = task.kind === 'web_video' && task.error ? videoMessage(task.error) : task.error?.trim();
  return { title: t('download.status.failed'), body: error ? t('download.notification.failed', { name, error: xunleiDownloadMessage(error) }) : name };
}

export function shouldNotifyTask(task: QueueTask, previousStatus: TaskStatus | undefined): boolean {
  return notificationForTask(task) !== null && previousStatus !== task.status;
}

async function hasNotificationPermission(): Promise<boolean> {
  if (!permissionPromise) {
    permissionPromise = import('@tauri-apps/plugin-notification').then(async ({ isPermissionGranted, requestPermission }) => {
      if (await isPermissionGranted()) return true;
      const permission = await requestPermission();
      return permission === 'granted';
    }).catch(() => false);
  }
  return permissionPromise;
}

export async function sendDownloadNotification(task: QueueTask): Promise<void> {
  const notification = notificationForTask(task);
  if (!notification || !(await hasNotificationPermission())) return;
  try {
    const { sendNotification } = await import('@tauri-apps/plugin-notification');
    await sendNotification(notification);
  } catch {
    // A disabled OS notification service must not affect the download engine.
  }
}
