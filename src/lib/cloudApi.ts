import { Channel, invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import type { LocalUploadListing } from './localUpload';
import type { CloudAccountCapacity, CloudAuthorizationStateEvent, CloudCapacityInvalidated, CloudUploadAction, CloudUploadProgress } from './cloudTypes';
import type { DuplicateDownloadState, QueueTask, TaskActionResult } from './types';
import type { ProviderDescriptor, CloudAccount, CloudAuthorization, CloudAuthorizationCapabilities, CloudCredentials, CloudDirectory, CloudDirectoryDownloadFailure, CloudDirectoryDownloadOutcome, CloudFile, CloudProvider, CloudShare, CloudShareCancelOutcome, CloudShareOptions, CloudUploadOutcome } from './cloudTypes';

export const getCloudProviderCatalog = () => invoke<ProviderDescriptor[]>('cloud_provider_catalog');

export const getCloudAuthorizationCapabilities = (provider: CloudProvider) => invoke<CloudAuthorizationCapabilities>('cloud_provider_capabilities', { provider });
export const beginCloudAuthorization = (provider: CloudProvider) => invoke<CloudAuthorization>('cloud_begin_authorization', { provider });
export const openCloudOperationVerification = (provider: CloudProvider, expectedAccountId: string, locale?: string) => invoke<void>('cloud_open_operation_verification', { provider, expectedAccountId, locale });
export const completeCloudOperationVerification = (provider: CloudProvider, expectedAccountId: string) => invoke<CloudAccount>('cloud_complete_operation_verification', { provider, expectedAccountId });
export const completeCloudAuthorization = (provider: CloudProvider) => invoke<CloudAccount>('cloud_complete_authorization', { provider });
export const getCompletedCloudAuthorization = (provider: CloudProvider) => invoke<CloudAccount | null>('cloud_completed_authorization', { provider });
const authorizationSubscribers = new Set<{ provider: CloudProvider; receive: (event: CloudAuthorizationStateEvent) => void }>();
let authorizationListener: Promise<() => void> | null = null;
export async function subscribeCloudAuthorization(provider: CloudProvider, receive: (event: CloudAuthorizationStateEvent) => void): Promise<() => void> {
  const subscriber = { provider, receive };
  authorizationSubscribers.add(subscriber);
  const subscription = authorizationListener ??= listen<CloudAuthorizationStateEvent>('cloud-authorization-state', event => {
    for (const observer of authorizationSubscribers) {
      if (event.payload.provider === observer.provider) observer.receive(event.payload);
    }
  });
  let unlisten: () => void;
  try { unlisten = await subscription; }
  catch (error) {
    authorizationSubscribers.delete(subscriber);
    if (authorizationListener === subscription) authorizationListener = null;
    throw error;
  }
  return () => {
    if (!authorizationSubscribers.delete(subscriber)) return;
    if (authorizationSubscribers.size === 0 && authorizationListener === subscription) {
      authorizationListener = null;
      unlisten();
    }
  };
}
export const reauthorizeFailedCloudTask = (taskId: string) => invoke<CloudAuthorization>('cloud_reauthorize_failed_task', { taskId });
export const loginCloudWithPassword = (provider: CloudProvider, credentials: CloudCredentials) => invoke<CloudAccount>('cloud_login_password', { provider, ...credentials });
export const getSavedCloudCredentials = (provider: CloudProvider) => invoke<import('./cloudTypes').SavedCloudCredentialInfo | null>('cloud_saved_password_credentials', { provider });
export const getCloudAccount = (provider: CloudProvider) => invoke<CloudAccount>('cloud_account', { provider });
export const getCloudAccountCapacity = (provider: CloudProvider) => invoke<CloudAccountCapacity>('cloud_account_capacity', { provider });
const capacitySubscribers = new Set<{ provider: CloudProvider; receive: (event: CloudCapacityInvalidated) => void }>();
let capacityListener: Promise<() => void> | null = null;
export async function subscribeCloudCapacityInvalidated(provider: CloudProvider, receive: (event: CloudCapacityInvalidated) => void): Promise<() => void> {
  const subscriber = { provider, receive };
  capacitySubscribers.add(subscriber);
  const subscription = capacityListener ??= listen<CloudCapacityInvalidated>('cloud-capacity-invalidated', event => {
    for (const observer of capacitySubscribers) {
      if (event.payload.provider === observer.provider) observer.receive(event.payload);
    }
  });
  let unlisten: () => void;
  try { unlisten = await subscription; }
  catch (error) {
    capacitySubscribers.delete(subscriber);
    if (capacityListener === subscription) capacityListener = null;
    throw error;
  }
  return () => {
    if (!capacitySubscribers.delete(subscriber)) return;
    if (capacitySubscribers.size === 0 && capacityListener === subscription) {
      capacityListener = null;
      unlisten();
    }
  };
}
export async function listCloudDirectory(provider: CloudProvider, parentId?: string): Promise<CloudDirectory> {
  if (parentId === undefined) {
    const descriptor = (await getCloudProviderCatalog()).find(item => item.provider === provider);
    if (!descriptor) throw { code: 'provider_unsupported', message: '网盘目录中没有此服务商' };
    parentId = descriptor.rootDirectoryId;
  }
  return invoke<CloudDirectory>('cloud_list_directory', { provider, parentId });
}
export const createCloudDirectory = (provider: CloudProvider, parentId: string, name: string) => invoke<CloudFile>('cloud_create_directory', { provider, parentId, name });
export const renameCloudFile = (provider: CloudProvider, fileId: string, name: string) => invoke<void>('cloud_rename', { provider, fileId, name });
export const moveCloudFiles = (provider: CloudProvider, fileIds: string[], targetParentId: string) => invoke<void>('cloud_move', { provider, fileIds, targetParentId });
export const copyCloudFiles = (provider: CloudProvider, fileIds: string[], targetParentId: string) => invoke<void>('cloud_copy', { provider, fileIds, targetParentId });
export const trashCloudFiles = (provider: CloudProvider, fileIds: string[]) => invoke<void>('cloud_trash', { provider, fileIds });
/** Permanently deletes files already displayed inside a provider recycle bin. */
export const deleteCloudFiles = (provider: CloudProvider, fileIds: string[]) => invoke<void>('cloud_delete', { provider, fileIds });
/** Restores files already displayed inside a provider recycle bin. */
export const restoreCloudFiles = (provider: CloudProvider, fileIds: string[]) => invoke<void>('cloud_restore', { provider, fileIds });
export const createCloudShare = (provider: CloudProvider, fileIds: string[], options: CloudShareOptions) => invoke<CloudShare>('cloud_create_share', { provider, fileIds, options });
export const listCloudShares = (provider: CloudProvider) => invoke<CloudShare[]>('cloud_list_shares', { provider });
export const cancelCloudShares = (provider: CloudProvider, shareIds: string[]) => invoke<CloudShareCancelOutcome>('cloud_cancel_shares', { provider, shareIds });
export const updateCloudSharePassCode = (provider: CloudProvider, shareId: string, passCode: string) => invoke<void>('cloud_update_share_pass_code', { provider, shareId, passCode });
type NativeCloudDownload = QueueTask & { duplicate?: DuplicateDownloadState; queueTask?: QueueTask };
type NativeCloudDirectoryDownload = {
  task: NativeCloudDownload;
  queuedCount: number;
  failures: CloudDirectoryDownloadFailure[];
};
function cloudDownloadResult(result: NativeCloudDownload): TaskActionResult {
  const { duplicate, queueTask, ...task } = result;
  return duplicate
    ? { duplicate: { task, state: duplicate }, ...(queueTask ? { queueTask } : {}) }
    : { task, ...(queueTask ? { queueTask } : {}) };
}
export const downloadCloudFile = (provider: CloudProvider, fileId: string, fileName: string, privateDownload = false): Promise<TaskActionResult> => invoke<NativeCloudDownload>('cloud_download_url', {
  provider,
  fileId,
  fileName,
  ...(privateDownload ? { privateDownload: true } : {})
})
  .then(cloudDownloadResult);
export const downloadCloudDirectory = (provider: CloudProvider, directoryId: string, directoryName: string, privateDownload = false): Promise<CloudDirectoryDownloadOutcome> => invoke<NativeCloudDirectoryDownload>('cloud_download_directory', {
  provider,
  directoryId,
  directoryName,
  ...(privateDownload ? { privateDownload: true } : {})
})
  .then((result) => ({ ...result, task: cloudDownloadResult(result.task) }));
export const logoutCloud = (provider: CloudProvider) => invoke<void>('cloud_logout', { provider });
async function invokeCloudUpload(command: string, args: Record<string, unknown>, receive?: (progress: CloudUploadProgress) => void): Promise<CloudUploadOutcome> {
  const uploadId = crypto.randomUUID();
  const onProgress = new Channel<CloudUploadProgress>();
  let active = true;
  onProgress.onmessage = progress => { if (active && progress.uploadId === uploadId && progress.provider === args.provider) receive?.(progress); };
  try {
    return await invoke<CloudUploadOutcome>(command, { ...args, uploadId, onProgress });
  } finally {
    active = false;
  }
}
export const controlCloudUpload = (provider: CloudProvider, uploadId: string, action: CloudUploadAction) => invoke<void>('cloud_control_upload', { provider, uploadId, action });
export const uploadCloudFiles = (provider: CloudProvider, parentId: string, paths?: string[], onProgress?: (progress: CloudUploadProgress) => void) => invokeCloudUpload('cloud_upload_files', { provider, parentId, paths: paths ?? null }, onProgress);
export const listLocalUploadEntries = (path?: string) => invoke<LocalUploadListing>('local_upload_entries', { path: path ?? null });
export const pickAndUploadFiles = (provider: CloudProvider, parentId: string) => uploadCloudFiles(provider, parentId);
export const uploadCloudDirectory = (provider: CloudProvider, parentId: string, path?: string, onProgress?: (progress: CloudUploadProgress) => void) => invokeCloudUpload('cloud_upload_directory', { provider, parentId, path: path ?? null }, onProgress);
export const pickAndUploadDirectory = (provider: CloudProvider, parentId: string) => uploadCloudDirectory(provider, parentId);

export async function pickCloudUploadFiles(): Promise<string[]> {
  const { open } = await import('@tauri-apps/plugin-dialog');
  const selected = await open({ directory: false, multiple: true, title: '选择上传文件' });
  if (Array.isArray(selected)) return selected;
  return selected ? [selected] : [];
}

export async function pickCloudUploadDirectory(): Promise<string | null> {
  const { open } = await import('@tauri-apps/plugin-dialog');
  const selected = await open({ directory: true, multiple: false, title: '选择上传目录' });
  return typeof selected === 'string' ? selected : null;
}
