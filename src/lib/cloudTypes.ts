import type { TaskActionResult } from './types';

export type CloudProvider = 'quark' | 'pikpak' | 'baidu' | 'tianyi' | 'aliyun' | 'xunlei' | 'uc';
export type CloudAuthMethod = 'web' | 'password';

export interface ProviderCapabilities {
  list: boolean; createDirectory: boolean; rename: boolean; move: boolean; copy: boolean;
  trash: boolean; delete: boolean; restore: boolean; download: boolean; upload: boolean;
  shareInspect: boolean; shareTransfer: boolean; recordedTransferResolve: boolean;
  shareCreate: boolean; shareList: boolean; shareCancel: boolean; shareUpdatePassCode?: boolean;
}
export interface DownloadConcurrencyPolicy {
  maxConnectionsPerFile: number;
  maxConcurrentTasks: number;
}
export type SharePassCodeCharacterSet = 'any' | 'asciiAlphanumeric';
export interface SharePassCodeConstraint {
  required?: boolean;
  minLength: number;
  maxLength: number;
  characterSet: SharePassCodeCharacterSet;
  hint: string;
}
export interface ProviderDescriptor {
  provider: CloudProvider;
  displayName: string;
  shareDisplayName: string;
  uploadDisplayName: string;
  shareDirectoryName: string;
  rootDirectoryId: string;
  recycleBinDirectoryId: string;
  authorization: CloudAuthorizationCapabilities;
  authorizationUrl: string;
  passwordLoginHosts: readonly string[];
  capabilities: ProviderCapabilities;
  shareHosts: readonly string[];
  sharePatterns: readonly string[];
  maxUploadConcurrency: number;
  downloadConcurrency: DownloadConcurrencyPolicy;
  sharePassCodeConstraint?: SharePassCodeConstraint;
}

export interface CloudAccount { provider: CloudProvider; id: string; displayName: string; authMethod: CloudAuthMethod; expiresAt?: string; }
export type CloudStorageCapacity =
  | { kind: 'limited'; totalBytes: number; usedBytes: number }
  | { kind: 'unlimited'; usedBytes: number };
export interface CloudAccountCapacity {
  provider: CloudProvider;
  accountId: string;
  capacity: CloudStorageCapacity;
  updatedAt: number;
}
export type CloudCapacityChangeReason = 'upload' | 'copy' | 'transfer' | 'trash' | 'restore' | 'delete';
export interface CloudCapacityInvalidated {
  provider: CloudProvider;
  accountId: string;
  reason: CloudCapacityChangeReason;
}
export interface CloudAuthorizationStateEvent { provider: CloudProvider; state: 'Disconnected' | 'Restoring' | 'Ready' | 'Refreshing' | 'NeedsSilentRelogin' | 'NeedsInteractiveAuth' | 'RiskBlocked'; account: CloudAccount | null; }
export interface CloudSessionPersistenceWarning { provider: CloudProvider; operationSucceeded: boolean; }
export interface CloudFile { id: string; parentId: string; name: string; kind: 'file' | 'folder'; size?: number; modifiedAt?: string; shareId?: string; shareUrl?: string; sharePassCode?: string; }
export interface CloudDirectory { id: string; items: CloudFile[]; }
export interface CloudShareOptions { name: string; passCode?: string; generatePassCode?: boolean; expiresInDays?: number; }
export interface CloudShareCancelFailure { id: string; code: string; message: string; }
export interface CloudShareCancelOutcome { cancelled: readonly string[]; failures: readonly CloudShareCancelFailure[]; }
export interface CloudShare { id: string; url: string; name: string; kind: 'file' | 'folder'; sourceFileIds: readonly string[]; passCode?: string; createdAt?: string; expiresAt?: string; }
export interface CloudUploadFailure { inputIndex?: number; itemName: string; code: string; message: string; }
export interface CloudUploadOutcome {
  uploaded: readonly CloudFile[];
  failures: readonly CloudUploadFailure[];
  cancelled?: boolean;
}
export type CloudUploadAction = 'pause' | 'resume' | 'cancel';
export interface CloudUploadProgress {
  uploadId: string;
  provider: CloudProvider;
  phase: 'preparing' | 'uploading' | 'finalizing' | 'paused' | 'cancelling' | 'cancelled' | 'completed' | 'failed';
  totalBytes: number;
  uploadedBytes: number;
  speedBytesPerSecond: number;
  totalFiles: number;
  completedFiles: number;
  failedItems: number;
  currentFiles: string[];
  progress: number;
}
export interface CloudAuthorization { provider: CloudProvider; windowLabel: string; instructions: string; completed?: boolean; }
export type CloudPasswordLoginMode = 'none' | 'rustDirect' | 'officialWebView';
export interface CloudAuthorizationCapabilities { provider: CloudProvider; web: boolean; password: boolean; passwordLoginMode: CloudPasswordLoginMode; }
export interface SavedCloudCredentialInfo { username: string; hasSavedPassword: boolean; }
export type CloudCredentials = { username: string } & (
  | { password: string; useSavedPassword?: false }
  | { password?: never; useSavedPassword: true }
);
export interface CloudDirectoryDownloadFailure { relativePath: string; code: string; message: string; }
export interface CloudDirectoryDownloadOutcome {
  task: TaskActionResult;
  queuedCount: number;
  failures: readonly CloudDirectoryDownloadFailure[];
}
