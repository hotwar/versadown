export type TaskStatus = 'draft' | 'parsing' | 'awaiting_user' | 'queued' | 'downloading' | 'processing' | 'completed' | 'failed' | 'cancelled' | 'recycled' | 'paused';
export type TaskKind = 'file' | 'folder' | 'magnet' | 'torrent' | 'ed2k' | 'ftp' | 'hls' | 'dash' | 'cloud' | 'web_video';
export type DuplicateDownloadState = 'existing_active' | 'resume_available' | 'completed_files_present' | 'completed_files_missing' | 'overwrite_required';
export type TaskCommand = 'pause' | 'resume' | 'cancel' | 'retry' | 'reauthorize' | 'retry_cleanup' | 'open_directory' | 'open_file' | 'play_videos' | 'copy_link' | 'details' | 'delete' | 'move_to_private' | 'move_to_normal';

export interface QueueTask {
  id: string;
  name: string;
  kind: TaskKind;
  status: TaskStatus;
  progress: number;
  speed?: string;
  download_speed?: number;
  upload_speed?: number;
  size?: string;
  eta?: string;
  eta_seconds?: number;
  connections?: number;
  discovered_nodes?: number;
  statusMessage?: string;
  source?: string;
  children?: readonly QueueTask[];
  error?: string;
  failure_code?: string;
  reauthorization_required?: boolean;
  cleanup_pending?: boolean;
  aria_gid?: string;
  followed_by_gid?: string;
  metadata_gid?: string;
  total_length?: number;
  completed_length?: number;
  aria_error_code?: string;
  created_at?: number;
  updated_at?: number;
  completed_at?: number;
  p2p_sharing?: boolean;
  is_private?: boolean;
  post_processing_note?: string;
  files?: readonly DownloadFile[];
  file_index?: number;
  selected?: boolean;
}

/**
 * Permissive input shape for the task event boundary. Normalized UI state
 * always uses QueueTask and therefore exposes only the current wire names.
 */
export interface QueueTaskInput extends Omit<QueueTask, 'children'> {
  children?: readonly QueueTaskInput[];
  speedBytes?: number;
  completedAt?: number;
  isPrivate?: boolean;
}

export interface DownloadFile {
  index: number;
  path: string;
  length: number;
  completed_length: number;
  selected: boolean;
}

export interface DuplicateDownload {
  task: QueueTask;
  state: DuplicateDownloadState;
}

export interface TaskActionResult {
  task?: QueueTask;
  duplicate?: DuplicateDownload;
  /** Durable cloud-directory actions return their authoritative recursive root here. */
  queueTask?: QueueTask;
}

export interface CloudShareBatchFailure {
  selectedId: string;
  code: string;
  message: string;
}

export interface CloudShareBatchResult {
  outcomes: TaskActionResult[];
  failures: CloudShareBatchFailure[];
}

export interface DownloadGlobalStat {
  download_speed: number;
  upload_speed: number;
  num_active: number;
  num_waiting?: number;
  num_stopped?: number;
  num_stopped_total?: number;
}

export interface DownloadEngineState {
  available: boolean;
  retrying: boolean;
  error?: string;
}

export interface ConfirmResource {
  id: string;
  name: string;
  size: string;
  selected: boolean;
  index?: number;
}

export interface VideoFormat {
  id: string;
  width: number;
  height: number;
  ext: string;
  bytes: number | null;
  estimated: boolean;
}

export interface VideoPreview {
  previewId: string;
  pageUrl: string;
  title: string;
  site: string;
  duration: number | null;
  thumbnail: string | null;
  formats: VideoFormat[];
}

export interface ParseResult {
  video?: VideoPreview;
  taskId: string;
  title?: string;
  requiresConfirmation: boolean;
  resources: ConfirmResource[];
  processing: string[];
  discoveredPassword?: string;
  isBitTorrent?: boolean;
  /** True only until the complete selectable file list is available. */
  fileListPending?: boolean;
}

export type DownloadInputKind = 'direct' | 'web_video' | 'ed2k' | 'ftp' | 'hls' | 'dash' | 'magnet' | 'torrent' | 'thunder' | 'cloud_share' | 'quark_share' | 'pik_pak_share';

export type ScanDownloadResult =
  | { input: string; valid: true; kind: DownloadInputKind; canProbeVideo?: boolean; message?: never }
  | { input: string; valid: false; kind?: never; message: string };

export type CloudShareProvider = 'quark' | 'pikpak' | 'baidu' | 'tianyi' | 'aliyun' | 'xunlei' | 'uc';

export interface CloudShareAccessCodeRequired {
  provider: CloudShareProvider;
  message: string;
}

export type ParseDownloadResult =
  | { task: QueueTask; confirmation?: never; shareAccessCodeRequired?: never; duplicate?: never }
  | { confirmation: ParseResult; task?: never; shareAccessCodeRequired?: never; duplicate?: never }
  | { shareAccessCodeRequired: CloudShareAccessCodeRequired; task?: never; confirmation?: never; duplicate?: never }
  | { duplicate: DuplicateDownload; task?: never; confirmation?: never; shareAccessCodeRequired?: never };

export type ProxyProtocol = 'http' | 'https';
export type LauncherVisibility = 'always' | 'downloading' | 'hidden';

export interface ProxySettings {
  enabled: boolean;
  protocol: ProxyProtocol;
  host: string;
  port: number;
  username: string;
  password: string;
}

export interface AiSettings {
  enabled: boolean;
  baseUrl: string;
  model: string;
  apiKeyConfigured: boolean;
}

export interface AiSettingsInput {
  enabled: boolean;
  baseUrl: string;
  model: string;
  /** Empty keeps an already saved local secret unchanged. */
  apiKey: string;
}

export interface MagnetMetadataSettings {
  enabled: boolean;
  baseUrl: string;
}

export interface Aria2Settings {
  rpcPort: number;
  rpcToken: string;
  maxConcurrentDownloads: number;
  maxConnectionsPerServer: number;
  btSeedRatio: number;
  debugLogging: boolean;
  overallDownloadLimit: number;
  p2pUploadLimit: number;
}

export interface AppSettings {
  downloadDirectory: string;
  launcherVisibility: LauncherVisibility;
  proxy: ProxySettings;
  ai: AiSettings;
  magnetMetadata: MagnetMetadataSettings;
  aria2: Aria2Settings;
  deleteTransferredShareFilesAfterDownload: boolean;
  downloadStatusNotifications?: boolean;
  associateDownloadLinks?: boolean;
  privacyDownloadEnabled?: boolean;
  privacyPasswordConfigured?: boolean;
  cloudUploadLimit?: number;
}

export interface PrivacyTaskVisibility {
  enabled: boolean;
  unlocked: boolean;
}

export interface AppUpdate {
  version: string;
  notes: string;
  downloadUrl: string;
}
