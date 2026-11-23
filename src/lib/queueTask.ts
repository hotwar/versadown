import { t } from '../i18n';
import { shareFailureMessage } from '../i18n/shareFailureMessage';
import { xunleiDownloadMessage } from '../i18n/xunleiDownloadMessage';
import type { QueueTask, QueueTaskInput } from './types';

/** Converts legacy event fields at the UI boundary into the canonical task shape. */
export function normalizeQueueTask(input: QueueTaskInput): QueueTask {
  const { children, speedBytes, completedAt, isPrivate, ...task } = input;
  return {
    ...task,
    ...(task.download_speed === undefined && speedBytes !== undefined ? { download_speed: speedBytes } : {}),
    ...(task.completed_at === undefined && completedAt !== undefined ? { completed_at: completedAt } : {}),
    ...(task.is_private === undefined && isPrivate !== undefined ? { is_private: isPrivate } : {}),
    ...(children !== undefined ? { children: normalizeQueueTasks(children) } : {}),
  };
}

export function normalizeQueueTasks(tasks: readonly QueueTaskInput[]): QueueTask[] {
  return tasks.map(normalizeQueueTask);
}

export function downloadErrorMessage(message: string): string {
  if (message === 'download.error.transferredFileUnavailable') return t(message);
  return shareFailureMessage(message) ?? xunleiDownloadMessage(message);
}

export function isPrivateTask(task: QueueTask): boolean {
  return task.is_private === true;
}
