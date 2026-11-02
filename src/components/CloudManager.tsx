import { FormEvent, KeyboardEvent as ReactKeyboardEvent, MouseEvent as ReactMouseEvent, useEffect, useLayoutEffect, useRef, useState, type PointerEvent as ReactPointerEvent } from 'react';
import { createPortal } from 'react-dom';
import { LocalUploadPicker } from './LocalUploadPicker';
import type { LocalUploadListing } from '../lib/localUpload';
import { CloudCapacityController, formatCloudCapacity, type CloudCapacityViewState } from '../lib/cloudCapacity';
import type { CloudUploadAction, CloudUploadProgress } from '../lib/cloudTypes';
import type { CloudAuthorizationStateEvent, CloudCapacityInvalidated } from '../lib/cloudTypes';
import type { ProviderDescriptor, CloudAccount, CloudAuthorization, CloudAuthorizationCapabilities, CloudCredentials, CloudDirectory, CloudDirectoryDownloadFailure, CloudDirectoryDownloadOutcome, CloudFile, CloudProvider, CloudShare, CloudShareOptions, CloudUploadOutcome, SharePassCodeConstraint } from '../lib/cloudTypes';
import { t, useTranslation } from '../i18n';
import { cloudProviderLabel } from '../i18n/cloudProviderLabel';

export type { CloudAccount, CloudAuthorization, CloudAuthorizationCapabilities, CloudCredentials, CloudDirectory, CloudDirectoryDownloadOutcome, CloudFile, CloudProvider } from '../lib/cloudTypes';

export interface CloudManagerClient {
  getCloudProviderCatalog(): Promise<readonly ProviderDescriptor[]>;
  getCloudAuthorizationCapabilities(provider: CloudProvider): Promise<CloudAuthorizationCapabilities>;
  beginCloudAuthorization(provider: CloudProvider): Promise<CloudAuthorization>;
  openCloudOperationVerification?(provider: CloudProvider, expectedAccountId: string, locale?: string): Promise<void>;
  completeCloudOperationVerification?(provider: CloudProvider, expectedAccountId: string): Promise<CloudAccount>;
  completeCloudAuthorization(provider: CloudProvider): Promise<CloudAccount>;
  getCompletedCloudAuthorization?(provider: CloudProvider): Promise<CloudAccount | null>;
  subscribeCloudAuthorization?(provider: CloudProvider, receive: (event: CloudAuthorizationStateEvent) => void): Promise<() => void>;
  loginCloudWithPassword(provider: CloudProvider, credentials: CloudCredentials): Promise<CloudAccount>;
  getSavedCloudCredentials?(provider: CloudProvider): Promise<import('../lib/cloudTypes').SavedCloudCredentialInfo | null>;
  getCloudAccount(provider: CloudProvider): Promise<CloudAccount>;
  getCloudAccountCapacity?(provider: CloudProvider): Promise<import('../lib/cloudTypes').CloudAccountCapacity>;
  subscribeCloudCapacityInvalidated?(provider: CloudProvider, receive: (event: CloudCapacityInvalidated) => void): Promise<() => void>;
  listCloudDirectory(provider: CloudProvider, parentId?: string): Promise<CloudDirectory>;
  createCloudDirectory(provider: CloudProvider, parentId: string, name: string): Promise<CloudFile | void>;
  renameCloudFile(provider: CloudProvider, fileId: string, name: string): Promise<void>;
  moveCloudFiles(provider: CloudProvider, fileIds: string[], targetParentId: string): Promise<void>;
  copyCloudFiles(provider: CloudProvider, fileIds: string[], targetParentId: string): Promise<void>;
  trashCloudFiles(provider: CloudProvider, fileIds: string[]): Promise<void>;
  deleteCloudFiles(provider: CloudProvider, fileIds: string[]): Promise<void>;
  restoreCloudFiles(provider: CloudProvider, fileIds: string[]): Promise<void>;
  createCloudShare(provider: CloudProvider, fileIds: string[], options: CloudShareOptions): Promise<CloudShare>;
  listCloudShares(provider: CloudProvider): Promise<readonly CloudShare[]>;
  cancelCloudShares(provider: CloudProvider, shareIds: string[]): Promise<void>;
  updateCloudSharePassCode?(provider: CloudProvider, shareId: string, passCode: string): Promise<void>;
  downloadCloudFile(provider: CloudProvider, fileId: string, fileName: string, privateDownload?: boolean): Promise<unknown>;
  downloadCloudDirectory(provider: CloudProvider, directoryId: string, directoryName: string, privateDownload?: boolean): Promise<CloudDirectoryDownloadOutcome>;
  logoutCloud(provider: CloudProvider): Promise<void>;
  pickAndUploadFiles(provider: CloudProvider, parentId: string): Promise<CloudUploadOutcome>;
  pickAndUploadDirectory(provider: CloudProvider, parentId: string): Promise<CloudUploadOutcome>;
  pickCloudUploadFiles?(): Promise<string[]>;
  pickCloudUploadDirectory?(): Promise<string | null>;
  uploadCloudFiles?(provider: CloudProvider, parentId: string, paths: string[], onProgress?: (progress: CloudUploadProgress) => void): Promise<CloudUploadOutcome>;
  uploadCloudDirectory?(provider: CloudProvider, parentId: string, path: string, onProgress?: (progress: CloudUploadProgress) => void): Promise<CloudUploadOutcome>;
  controlCloudUpload?(provider: CloudProvider, uploadId: string, action: CloudUploadAction): Promise<void>;
  listLocalUploadEntries?(path?: string): Promise<LocalUploadListing>;
}

export interface CloudManagerProps {
  descriptor: ProviderDescriptor;
  recoveryFailed?: boolean;
  provider: CloudProvider;
  client?: CloudManagerClient;
  initialAccount?: CloudAccount | null;
  onDownloadTask?(task: unknown): void;
  authorizationOnly?: boolean;
  restoreOnMount?: boolean;
  showSessionActions?: boolean;
  workspaceVisible?: boolean;
  privacyDownloadEnabled?: boolean;
  authorizationCardDrag?: {
    dragging: boolean;
    dropTarget: boolean;
    onPointerDown(event: ReactPointerEvent<HTMLElement>): void;
  };
  onAccountAuthorized?(account: CloudAccount): void;
  onAccountLoggedOut?(provider: CloudProvider): void;
}

type DirectoryState = 'idle' | 'loading' | 'ready' | 'empty' | 'error' | 'expired';
type Editor = 'create' | 'rename' | null;
type DestinationAction = 'move' | 'copy';
type OperationState = { kind: 'working' | 'success' | 'error' | 'cancelled'; message: string; operationVerification?: boolean; verificationOpened?: boolean; uploadProgress?: CloudUploadProgress } | null;
type DirectoryRefreshConfirmation = (directory: CloudDirectory) => boolean;
type PendingAuthorizationAction = { accountId: string; name: string; resume: () => Promise<void>; showOperation: boolean };
interface Breadcrumb { id: string; name: string }
type CloudMenuAction = 'open' | 'restore' | 'download' | 'rename' | 'move' | 'copy' | 'share' | 'copyShareLink' | 'updateSharePassCode' | 'cancelShare' | 'delete' | 'create' | 'upload' | 'refresh';
type CloudContextMenuState = { kind: 'selection'; item: CloudFile; x: number; y: number } | { kind: 'background'; x: number; y: number };
const initialAuthorizationStatus = t('cloud.authorization.notAuthorized');
const expiredAuthorizationStatus = t('cloud.authorization.expired');
// The upload response confirms that the provider accepted the file, but some
// drives index the file into the directory listing asynchronously. Always use
// fresh server listings and retry a bounded number of times instead of adding
// the response optimistically to the visible directory.
const DIRECTORY_INDEX_CONFIRMATION_DELAYS_MS = [0, 250, 750, 1_500] as const;
type CloudHeaderMenuAction = 'rename' | 'move' | 'copy' | 'share';
const MY_SHARES_DIRECTORY_ID = '__versa_down_my_shares__';

const authorizationWatchdogMs = 10_000;

const unavailableClient: CloudManagerClient = {
  getCloudProviderCatalog: () => Promise.reject(new Error('网盘连接器尚未接入')),
  getCloudAuthorizationCapabilities: () => Promise.reject(new Error('网盘连接器尚未接入')),
  beginCloudAuthorization: () => Promise.reject(new Error('网盘连接器尚未接入')),
  completeCloudAuthorization: () => Promise.reject(new Error('网盘连接器尚未接入')),
  loginCloudWithPassword: () => Promise.reject(new Error('网盘连接器尚未接入')),
  getSavedCloudCredentials: () => Promise.resolve(null),
  getCloudAccount: () => Promise.reject(new Error('网盘连接器尚未接入')),
  listCloudDirectory: () => Promise.reject(new Error('网盘连接器尚未接入')),
  createCloudDirectory: () => Promise.reject(new Error('网盘连接器尚未接入')),
  renameCloudFile: () => Promise.reject(new Error('网盘连接器尚未接入')),
  moveCloudFiles: () => Promise.reject(new Error('网盘连接器尚未接入')),
  copyCloudFiles: () => Promise.reject(new Error('网盘连接器尚未接入')),
  trashCloudFiles: () => Promise.reject(new Error('网盘连接器尚未接入')),
  deleteCloudFiles: () => Promise.reject(new Error('网盘连接器尚未接入')),
  restoreCloudFiles: () => Promise.reject(new Error('网盘连接器尚未接入')),
  createCloudShare: () => Promise.reject(new Error('网盘连接器尚未接入')),
  listCloudShares: () => Promise.reject(new Error('网盘连接器尚未接入')),
  cancelCloudShares: () => Promise.reject(new Error('网盘连接器尚未接入')),
  updateCloudSharePassCode: () => Promise.reject(new Error('网盘连接器尚未接入')),
  downloadCloudFile: () => Promise.reject(new Error('网盘连接器尚未接入')),
  downloadCloudDirectory: () => Promise.reject(new Error('网盘连接器尚未接入')),
  logoutCloud: () => Promise.reject(new Error('网盘连接器尚未接入')),
  pickAndUploadFiles: () => Promise.reject(new Error('网盘连接器尚未接入')),
  pickAndUploadDirectory: () => Promise.reject(new Error('网盘连接器尚未接入'))
};

function isCredentialExpired(error: unknown) {
  return typeof error === 'object' && error !== null && 'code' in error && (error as { code?: unknown }).code === 'credentials_expired';
}

function isUnauthorized(error: unknown) {
  return typeof error === 'object' && error !== null && 'code' in error && ['unauthorized', 'not_authorized'].includes(String((error as { code?: unknown }).code));
}

function isAuthorizationPending(error: unknown) {
  return typeof error === 'object' && error !== null && 'code' in error && ['authorization_pending', 'waiting', 'not_ready'].includes(String((error as { code?: unknown }).code));
}

function requiresOfficialWebAuthorization(error: unknown) {
  if (typeof error !== 'object' || error === null) return false;
  const code = 'code' in error ? String((error as { code?: unknown }).code).toLowerCase() : '';
  if (code === 'pikpak_operation_verification_required') return false;
  const message = errorMessage(error, '').toLowerCase();
  return ['web_auth_required', 'captcha_required', 'captcha_invalid', 'verification_required', 'web_verification_required', 'risk_verification_required'].includes(code)
    || /captcha|verification required|网页验证|安全验证|风险验证/.test(message);
}

function requiresPikpakOperationVerification(error: unknown) {
  return typeof error === 'object' && error !== null && 'code' in error
    && error.code === 'pikpak_operation_verification_required';
}

function requiresBaiduOperationVerification(error: unknown) {
  return typeof error === 'object' && error !== null && 'code' in error
    && error.code === 'baidu_operation_verification_required';
}

function baiduVerificationText() {
  return {
    required: t('cloud.verification.baidu.required'),
    open: t('cloud.verification.baidu.open'),
    opened: t('cloud.verification.baidu.opened'),
    complete: t('cloud.verification.baidu.complete'),
    completed: t('cloud.verification.baidu.completed'),
    completeFailed: t('cloud.verification.baidu.completeFailed'),
    accountMismatch: t('cloud.verification.baidu.accountMismatch'),
  };
}

function isSessionRenewedRetryOperation(error: unknown) {
  return typeof error === 'object' && error !== null && 'code' in error && (error as { code?: unknown }).code === 'session_renewed_retry_operation';
}

function isTerminalWebAuthorizationError(error: unknown) {
  return typeof error === 'object' && error !== null && 'code' in error && ['provider_error', 'invalid_response', 'credentials_expired'].includes(String((error as { code?: unknown }).code));
}

function errorMessage(error: unknown, fallback: string) {
  if (error instanceof Error && error.message) return error.message;
  if (typeof error === 'object' && error !== null && 'message' in error && typeof (error as { message?: unknown }).message === 'string') {
    return (error as { message: string }).message;
  }
  return fallback;
}

function readableSize(size?: number) {
  if (size === undefined) return '—';
  if (size < 1024) return `${size} B`;
  const units = ['KB', 'MB', 'GB', 'TB'];
  let value = size / 1024;
  let unitIndex = 0;
  while (value >= 1024 && unitIndex < units.length - 1) {
    value /= 1024;
    unitIndex += 1;
  }
  return `${value.toFixed(1)} ${units[unitIndex]}`;
}

function isValidDateTime(year: number, month: number, day: number, hour: number, minute: number, second: number) {
  if (year < 1 || year > 9_999 || month < 1 || month > 12 || day < 1 || hour > 23 || minute > 59 || second > 59) return false;
  const leapYear = year % 4 === 0 && (year % 100 !== 0 || year % 400 === 0);
  const daysInMonth = [31, leapYear ? 29 : 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
  return day <= daysInMonth[month - 1];
}

function formatLocalCloudDate(value: string) {
  const match = /^(\d{4})-(\d{2})-(\d{2})(?: (\d{2}):(\d{2})(?::(\d{2}))?)?$/.exec(value);
  if (!match) return null;
  const [, year, month, day, hour = '00', minute = '00', second = '00'] = match;
  if (!isValidDateTime(Number(year), Number(month), Number(day), Number(hour), Number(minute), Number(second))) return null;
  return `${year}-${month}-${day} ${hour}:${minute}:${second}`;
}

function formatCloudModifiedAt(value: unknown) {
  if (typeof value === 'string') {
    const localDate = formatLocalCloudDate(value);
    if (localDate) return localDate;
  }

  const numericValue = typeof value === 'number' ? value : typeof value === 'string' && /^-?\d+(?:\.\d+)?$/.test(value) ? Number(value) : NaN;
  const isoMatch = typeof value === 'string' ? /^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2})(?::(\d{2})(?:\.\d+)?)?(?:Z|[+-]\d{2}:?\d{2})$/i.exec(value) : null;
  const timestamp = Number.isFinite(numericValue)
    ? Math.abs(numericValue) < 100_000_000_000 ? numericValue * 1_000 : numericValue
    : isoMatch && isValidDateTime(Number(isoMatch[1]), Number(isoMatch[2]), Number(isoMatch[3]), Number(isoMatch[4]), Number(isoMatch[5]), Number(isoMatch[6] ?? '0')) ? Date.parse(isoMatch[0]) : NaN;
  const date = new Date(timestamp);
  if (Number.isNaN(date.getTime())) return '—';
  return `${date.getFullYear()}-${String(date.getMonth() + 1).padStart(2, '0')}-${String(date.getDate()).padStart(2, '0')} ${String(date.getHours()).padStart(2, '0')}:${String(date.getMinutes()).padStart(2, '0')}:${String(date.getSeconds()).padStart(2, '0')}`;
}

type CloudSortColumn = 'name' | 'size' | 'modifiedAt';
type CloudSortDirection = 'ascending' | 'descending';
type CloudSort = { column: CloudSortColumn; direction: CloudSortDirection };

const cloudNameCollator = new Intl.Collator('zh-CN', { numeric: true, sensitivity: 'base' });

function compareOptional<T>(left: T | undefined, right: T | undefined, compare: (left: T, right: T) => number) {
  if (left === undefined) return { comparison: right === undefined ? 0 : 1, missing: true };
  if (right === undefined) return { comparison: -1, missing: true };
  return { comparison: compare(left, right), missing: false };
}

function sortableCloudModifiedAt(value: unknown) {
  const formatted = formatCloudModifiedAt(value);
  return formatted === '—' ? undefined : formatted;
}

function sortableCloudSize(value: unknown) {
  return typeof value === 'number' && Number.isFinite(value) && value >= 0 ? value : undefined;
}

function sortedCloudFiles(items: readonly CloudFile[], sort: CloudSort | null) {
  if (!sort) return items;
  return items.map((item, index) => ({ item, index })).sort((left, right) => {
    const { comparison, missing } = sort.column === 'name'
      ? { comparison: cloudNameCollator.compare(left.item.name, right.item.name), missing: false }
      : sort.column === 'size'
        ? compareOptional(sortableCloudSize(left.item.size), sortableCloudSize(right.item.size), (a, b) => a - b)
        : compareOptional(
          sortableCloudModifiedAt(left.item.modifiedAt),
          sortableCloudModifiedAt(right.item.modifiedAt),
          (a, b) => a.localeCompare(b),
        );
    if (comparison === 0) return left.index - right.index;
    return sort.direction === 'ascending' || missing ? comparison : -comparison;
  }).map(({ item }) => item);
}

type CloudFileIconKind = CloudFile['kind'] | 'recycle-bin' | 'my-shares';

function CloudFileIcon({ kind }: { kind: CloudFileIconKind }) {
  return <svg aria-hidden="true" viewBox="0 0 24 24">{kind === 'recycle-bin'
    ? <><path d="M5 7h14M9 7V4h6v3M8 10v8M12 10v8M16 10v8M6.5 7l.8 14h9.4l.8-14" /></>
    : kind === 'my-shares'
    ? <><circle cx="18" cy="5" r="2.2" /><circle cx="6" cy="12" r="2.2" /><circle cx="18" cy="19" r="2.2" /><path d="m8 11 7.8-4.7M8 13l7.8 4.7" /></>
    : kind === 'folder'
    ? <><path d="M3.5 7.5h6l1.8 2h10.2v8.8a2.2 2.2 0 0 1-2.2 2.2H5.7a2.2 2.2 0 0 1-2.2-2.2z" /><path d="M3.5 7.5V5.7a2.2 2.2 0 0 1 2.2-2.2h4.1l1.8 2h6.7a2.2 2.2 0 0 1 2.2 2.2v1.8" /></>
    : <><path d="M6 2.8h7l5 5v13.4H6z" /><path d="M13 2.8v5h5M9 13h6M9 16.5h6" /></>}</svg>;
}

type CloudActionIconType = CloudMenuAction | 'details' | 'back' | 'clear' | 'pause' | 'resume' | 'cancel';

function CloudActionIcon({ action }: { action: CloudActionIconType }) {
  const paths: Record<CloudActionIconType, React.ReactNode> = {
    open: <><path d="M3.5 7.5h6l1.8 2h9.2v8.8a2.2 2.2 0 0 1-2.2 2.2H5.7a2.2 2.2 0 0 1-2.2-2.2z" /><path d="m12 13 2.5 2.5L18 12" /></>,
    restore: <><path d="M19 8a7.5 7.5 0 1 0 1 7" /><path d="m19 3 0 5-5 0" /></>,
    download: <><path d="M12 3v12m0 0 4-4m-4 4-4-4" /><path d="M5 20h14" /></>,
    rename: <><path d="m4 17-.7 3.7L7 20l10.8-10.8-3-3z" /><path d="m13.8 7.2 3 3" /></>,
    move: <><path d="M3.5 7.5h6l1.8 2h9.2v8.8a2.2 2.2 0 0 1-2.2 2.2H5.7a2.2 2.2 0 0 1-2.2-2.2z" /><path d="m10 14 2-2 2 2m-2-2v5" /></>,
    copy: <><rect x="8" y="8" width="11" height="11" rx="2" /><path d="M16 8V6a2 2 0 0 0-2-2H6a2 2 0 0 0-2 2v8a2 2 0 0 0 2 2h2" /></>,
    share: <><circle cx="18" cy="5" r="2.2" /><circle cx="6" cy="12" r="2.2" /><circle cx="18" cy="19" r="2.2" /><path d="m8 11 7.8-4.7M8 13l7.8 4.7" /></>,
    copyShareLink: <><path d="M10.2 13.8a4 4 0 0 0 5.7.1l2.1-2.1a4 4 0 0 0-5.7-5.7l-1.2 1.2" /><path d="M13.8 10.2a4 4 0 0 0-5.7-.1L6 12.2a4 4 0 0 0 5.7 5.7l1.2-1.2" /></>,
    cancelShare: <><circle cx="12" cy="12" r="8.5" /><path d="m8.5 8.5 7 7m0-7-7 7" /></>,
    updateSharePassCode: <><rect x="5" y="10" width="14" height="11" rx="2" /><path d="M8 10V7a4 4 0 0 1 8 0v3M12 14v3" /></>,
    details: <><circle cx="12" cy="12" r="9" /><path d="M12 11v6M12 7.5h.01" /></>,
    delete: <><path d="M5 7h14M9 7V4h6v3M8 10v8M12 10v8M16 10v8M6.5 7l.8 14h9.4l.8-14" /></>,
    create: <><path d="M3.5 7.5h6l1.8 2h9.2v8.8a2.2 2.2 0 0 1-2.2 2.2H5.7a2.2 2.2 0 0 1-2.2-2.2z" /><path d="M12 12v6M9 15h6" /></>,
    upload: <><path d="M12 16V4m0 0-4 4m4-4 4 4" /><path d="M5 20h14" /></>,
    refresh: <><path d="M19 8a7.5 7.5 0 1 0 1 7" /><path d="M19 3v5h-5" /></>,
    back: <path d="m14.5 5-7 7 7 7" />,
    clear: <><path d="m7 7 10 10" /><path d="m17 7-10 10" /></>,
    pause: <><path d="M8 5v14" /><path d="M16 5v14" /></>,
    resume: <path d="m9 6 9 6-9 6z" />,
    cancel: <><circle cx="12" cy="12" r="8" /><path d="m9 9 6 6M15 9l-6 6" /></>
  };
  return <svg aria-hidden="true" viewBox="0 0 24 24">{paths[action]}</svg>;
}

function CloudContextMenu({ state, selectedCount, recycleBin, virtualDirectory, myShares, canUpdateSharePassCode, onAction, onClose }: { state: CloudContextMenuState; selectedCount: number; recycleBin: boolean; virtualDirectory: boolean; myShares: boolean; canUpdateSharePassCode: boolean; onAction(action: CloudMenuAction): void; onClose(): void }) {
  const { t } = useTranslation();
  const menuRef = useRef<HTMLDivElement>(null);
  const items: Array<{ action: CloudMenuAction; label: Parameters<typeof t>[0]; danger?: boolean; separated?: boolean }> = state.kind === 'background'
    ? recycleBin || myShares ? [{ action: 'refresh', label: 'cloud.action.refresh' as const }] : [{ action: 'create', label: 'cloud.action.newFolder' as const }, { action: 'upload', label: 'cloud.action.upload' as const }, { action: 'refresh', label: 'cloud.action.refresh' as const, separated: true }]
    : virtualDirectory
      ? [{ action: 'open', label: 'cloud.action.open' as const }]
      : myShares
      ? [{ action: 'copyShareLink', label: 'cloud.action.copyShareLink' as const }, ...(canUpdateSharePassCode && selectedCount === 1 ? [{ action: 'updateSharePassCode' as const, label: 'cloud.share.changePassCode' as const }] : []), { action: 'cancelShare', label: 'cloud.action.cancelShare' as const, danger: true, separated: true }]
      : recycleBin
      ? [{ action: 'restore', label: 'cloud.action.restore' as const }, { action: 'delete', label: 'cloud.action.permanentDelete' as const, danger: true, separated: true }]
      : [...(selectedCount === 1 && state.item.kind === 'folder' ? [{ action: 'open' as const, label: 'cloud.action.open' as const }] : []), { action: 'download', label: 'download.action.download' as const }, ...(selectedCount === 1 ? [{ action: 'rename' as const, label: 'cloud.action.rename' as const, separated: true }] : []), { action: 'move', label: 'cloud.action.move' as const, separated: selectedCount > 1 }, { action: 'copy', label: 'cloud.action.copyTo' as const }, { action: 'share', label: 'cloud.action.share' as const, separated: true }, { action: 'delete', label: 'download.action.delete' as const, danger: true, separated: true }];
  const [menuSize, setMenuSize] = useState({ width: 210, height: items.length * 38 + (state.kind === 'selection' && selectedCount > 1 ? 24 : 14) });
  const left = Math.max(8, Math.min(state.x, window.innerWidth - menuSize.width - 8));
  const top = Math.max(8, Math.min(state.y, window.innerHeight - menuSize.height - 8));

  useLayoutEffect(() => {
    const rect = menuRef.current?.getBoundingClientRect();
    if (!rect?.width || !rect.height) return;
    setMenuSize((current) => current.width === rect.width && current.height === rect.height
      ? current
      : { width: rect.width, height: rect.height });
  }, [state.x, state.y, items.length]);

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

  function moveFocus(event: ReactKeyboardEvent<HTMLDivElement>) {
    if (!['ArrowDown', 'ArrowUp', 'Home', 'End'].includes(event.key)) return;
    event.preventDefault();
    const buttons = Array.from(menuRef.current?.querySelectorAll<HTMLButtonElement>('[role="menuitem"]') ?? []);
    const current = buttons.indexOf(document.activeElement as HTMLButtonElement);
    const next = event.key === 'Home' ? 0 : event.key === 'End' ? buttons.length - 1 : event.key === 'ArrowDown' ? (current + 1) % buttons.length : (current - 1 + buttons.length) % buttons.length;
    buttons[next]?.focus();
  }

  const title = state.kind === 'background' ? t('cloud.fileAreaMenu') : t('cloud.fileMenu', { name: state.item.name });
  return createPortal(<div ref={menuRef} className="cloud-context-menu" role="menu" aria-label={title} style={{ left, top }} onKeyDown={moveFocus}>
    {state.kind === 'selection' && selectedCount > 1 && <header><small>{t('cloud.itemsSelected', { count: selectedCount })}</small></header>}
    {items.map((item) => <div key={item.action} className={item.separated ? 'menu-separated' : undefined}><button role="menuitem" className={item.danger ? 'danger' : undefined} onClick={() => onAction(item.action)}><CloudActionIcon action={item.action} /><span>{t(item.label)}</span></button></div>)}
  </div>, document.body);
}

function CloudSelectionOverflowMenu({ anchor, singleSelection, recycleBin, disabled, onAction, onClose }: { anchor: { x: number; y: number }; singleSelection: boolean; recycleBin: boolean; disabled: boolean; onAction(action: CloudHeaderMenuAction): void; onClose(): void }) {
  const { t } = useTranslation();
  const menuRef = useRef<HTMLDivElement>(null);
  const items: Array<{ action: CloudHeaderMenuAction; label: Parameters<typeof t>[0]; separated?: boolean }> = [
    ...(recycleBin
      ? []
      : [
        ...(singleSelection ? [{ action: 'rename' as const, label: 'cloud.action.rename' as const }] : []),
        { action: 'move' as const, label: 'cloud.action.move' as const, separated: singleSelection },
        { action: 'copy' as const, label: 'cloud.action.copyTo' as const },
        { action: 'share' as const, label: 'cloud.action.share' as const, separated: true }
      ])
  ];
  const width = 180;
  const estimatedHeight = items.length * 38 + 14;
  const left = Math.max(8, Math.min(anchor.x - width, window.innerWidth - width - 8));
  const top = Math.max(8, Math.min(anchor.y, window.innerHeight - estimatedHeight - 8));

  useEffect(() => {
    menuRef.current?.querySelector<HTMLButtonElement>('[role="menuitem"]')?.focus();
    const closeForPointer = (event: PointerEvent) => { if (!menuRef.current?.contains(event.target as Node)) onClose(); };
    const closeForEscape = (event: KeyboardEvent) => { if (event.key === 'Escape') onClose(); };
    document.addEventListener('pointerdown', closeForPointer);
    document.addEventListener('keydown', closeForEscape);
    window.addEventListener('blur', onClose);
    window.addEventListener('resize', onClose);
    window.addEventListener('scroll', onClose, true);
    return () => {
      document.removeEventListener('pointerdown', closeForPointer);
      document.removeEventListener('keydown', closeForEscape);
      window.removeEventListener('blur', onClose);
      window.removeEventListener('resize', onClose);
      window.removeEventListener('scroll', onClose, true);
    };
  }, [onClose]);

  return createPortal(<div ref={menuRef} className="cloud-context-menu cloud-selection-overflow-menu" role="menu" aria-label={t('cloud.moreFileActions')} style={{ left, top }}>
    {items.map((item) => <div key={item.action} className={item.separated ? 'menu-separated' : undefined}><button role="menuitem" disabled={disabled} onClick={() => onAction(item.action)}><CloudActionIcon action={item.action} /><span>{t(item.label)}</span></button></div>)}
  </div>, document.body);
}

function useModalFocusTrap(containerRef: React.RefObject<HTMLElement | null>, initialFocusRef: React.RefObject<HTMLElement | null>, onClose: () => void) {
  const previousFocus = useRef<HTMLElement | null>(null);
  const closeCallback = useRef(onClose);
  closeCallback.current = onClose;

  useEffect(() => {
    previousFocus.current = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    initialFocusRef.current?.focus();
    const handleKey = (event: KeyboardEvent) => {
      if (event.key === 'Escape') {
        event.preventDefault();
        closeCallback.current();
        return;
      }
      if (event.key !== 'Tab') return;
      const focusable = Array.from(containerRef.current?.querySelectorAll<HTMLElement>('button:not(:disabled), [href], input:not(:disabled), select:not(:disabled), textarea:not(:disabled), [tabindex]:not([tabindex="-1"])') ?? []);
      if (!focusable.length) return;
      const first = focusable[0];
      const last = focusable[focusable.length - 1];
      if (focusable.length === 1 || (!event.shiftKey && document.activeElement === last) || (event.shiftKey && document.activeElement === first) || !containerRef.current?.contains(document.activeElement)) {
        event.preventDefault();
        (event.shiftKey ? last : first).focus();
      }
    };
    document.addEventListener('keydown', handleKey);
    return () => {
      document.removeEventListener('keydown', handleKey);
      if (previousFocus.current?.isConnected) previousFocus.current.focus();
    };
  }, [containerRef, initialFocusRef]);
}

async function copyCloudShareLink(value: string) {
  if (typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window) {
    const { writeText } = await import('@tauri-apps/plugin-clipboard-manager');
    await writeText(value);
    return;
  }
  await navigator.clipboard.writeText(value);
}

function cloudShareCopyUrl(url: string, passCode?: string): string {
  if (!passCode) return url;
  try {
    const parsed = new URL(url);
    // Baidu supports an access code in the URL itself. Other providers keep
    // their returned URL; the code remains a separate field in the dialog.
    if (parsed.hostname === 'pan.baidu.com') {
      parsed.searchParams.set('pwd', passCode);
      return parsed.toString();
    }
  } catch { /* Preserve the provider value if it is not an absolute URL. */ }
  return url;
}

function sharePassCodeHint(constraint: SharePassCodeConstraint): string {
  return constraint.characterSet === 'asciiAlphanumeric' && constraint.minLength === constraint.maxLength
    ? t('cloud.share.fixedCodeHint', { count: constraint.minLength }) : constraint.hint;
}

function sharePassCodeError(passCode: string, constraint?: SharePassCodeConstraint): string | null {
  const value = passCode.trim();
  if (!value) return constraint?.required ? sharePassCodeHint(constraint) : null;
  if (!constraint) return null;
  const hasValidLength = value.length >= constraint.minLength && value.length <= constraint.maxLength;
  const hasValidCharacters = constraint.characterSet !== 'asciiAlphanumeric' || /^[A-Za-z0-9]+$/.test(value);
  return hasValidLength && hasValidCharacters ? null : sharePassCodeHint(constraint);
}

function CloudShareDialog({ provider, items, share, busy, sharePassCodeConstraint, onCreate, onCopy, onClose, onUpdatePassCode }: { provider: CloudProvider; items: readonly CloudFile[]; share: CloudShare | null; busy: boolean; sharePassCodeConstraint?: SharePassCodeConstraint; onCreate(options: CloudShareOptions): void; onCopy(value: string): void; onClose(): void; onUpdatePassCode?(passCode: string): Promise<void> }) {
  const { t } = useTranslation();
  const dialogRef = useRef<HTMLElement>(null);
  const initialFocusRef = useRef<HTMLInputElement>(null);
  const [passCode, setPassCode] = useState('');
  const [generatePassCode, setGeneratePassCode] = useState(Boolean(sharePassCodeConstraint?.required));
  const [expiresInDays, setExpiresInDays] = useState('');
  const [updatedPassCode, setUpdatedPassCode] = useState(share?.passCode ?? '');
  const [savingPassCode, setSavingPassCode] = useState(false);
  const [updateNotice, setUpdateNotice] = useState<{ error: boolean; message: string } | null>(null);
  useEffect(() => { setUpdatedPassCode(share?.passCode ?? ''); setUpdateNotice(null); }, [share?.id]);
  useModalFocusTrap(dialogRef, initialFocusRef, () => { if (!savingPassCode) onClose(); });
  const updatedPassCodeError = sharePassCodeError(updatedPassCode, sharePassCodeConstraint);
  const updatePassCode = async (event: FormEvent) => {
    event.preventDefault();
    if (!onUpdatePassCode || !updatedPassCode.trim() || updatedPassCodeError || savingPassCode || busy) return;
    setSavingPassCode(true);
    setUpdateNotice(null);
    try {
      await onUpdatePassCode(updatedPassCode.trim());
      setUpdateNotice({ error: false, message: t('cloud.share.passCodeUpdated') });
    } catch (error) {
      setUpdateNotice({ error: true, message: t('cloud.share.updatePassCodeFailed', { detail: errorMessage(error, t('common.retry')) }) });
    } finally { setSavingPassCode(false); }
  };
  const title = items.length === 1 ? items[0]?.name ?? t('cloud.share.file') : t('cloud.share.multipleItems', { name: items[0]?.name ?? t('cloud.file'), count: items.length });
  const serverGeneratedPassCode = provider === 'pikpak';
  const alwaysGeneratedPassCode = provider === 'tianyi' || provider === 'xunlei';
  const expiryOptions = provider === 'tianyi' ? [['', t('cloud.share.permanent')], ['1', t('cloud.share.days', { count: 1 })], ['7', t('cloud.share.days', { count: 7 })]] : [['', t('cloud.share.permanent')], ['1', t('cloud.share.days', { count: 1 })], ['7', t('cloud.share.days', { count: 7 })], ['30', t('cloud.share.days', { count: 30 })]];
  const passCodeError = generatePassCode ? null : sharePassCodeError(passCode, sharePassCodeConstraint);
  const submit = (event: FormEvent) => {
    event.preventDefault();
    if (passCodeError) return;
    onCreate({ name: title, ...(generatePassCode ? { generatePassCode: true } : (passCode.trim() ? { passCode: passCode.trim() } : {})), ...(expiresInDays ? { expiresInDays: Number(expiresInDays) } : {}) });
  };
  return createPortal(<div className="cloud-confirm-backdrop">
    <section ref={dialogRef} className="cloud-share-dialog" role="dialog" aria-modal="true" aria-label={share ? t('cloud.share.created') : t('cloud.share.create')}>
      {share ? <>
        <h3>{t('cloud.share.created')}</h3>
        <p>{share.name}</p>
        <form className="cloud-operation-editor"><label>{t('cloud.share.link')}<input aria-label={t('cloud.share.link')} readOnly value={cloudShareCopyUrl(share.url, share.passCode)} /></label></form>
        {share.passCode && <p>{t('cloud.share.passCodeLabel')}：<b>{share.passCode}</b></p>}
        {onUpdatePassCode && <form className="cloud-operation-editor cloud-share-passcode-editor" onSubmit={updatePassCode}>
          <label>{t('cloud.share.changePassCode')}<input aria-label={t('cloud.share.newPassCode')} aria-invalid={Boolean(updatedPassCodeError)} disabled={savingPassCode || busy} value={updatedPassCode} maxLength={sharePassCodeConstraint?.maxLength ?? 32} onChange={(event) => { setUpdatedPassCode(event.target.value); setUpdateNotice(null); }} /></label>
          <button type="submit" className="secondary" disabled={savingPassCode || busy || !updatedPassCode.trim() || Boolean(updatedPassCodeError) || updatedPassCode.trim() === share.passCode}>{savingPassCode ? t('common.saving') : t('cloud.share.savePassCode')}</button>
          {updatedPassCodeError && <p role="alert">{updatedPassCodeError}</p>}
          {updateNotice && <p role={updateNotice.error ? 'alert' : 'status'}>{updateNotice.message}</p>}
        </form>}
        <button className="secondary" onClick={() => onCopy(cloudShareCopyUrl(share.url, share.passCode))}>{t('cloud.share.copyLink')}</button><button className="primary" disabled={savingPassCode} onClick={onClose}>{t('common.done')}</button>
      </> : <form onSubmit={submit}>
        <h3>{t('cloud.share.create')}</h3>
        <p>{t('cloud.share.description', { name: title, options: alwaysGeneratedPassCode ? t('cloud.share.generatedOptions') : sharePassCodeConstraint?.required ? t('cloud.share.requiredOptions', { hint: sharePassCodeHint(sharePassCodeConstraint) }) : sharePassCodeConstraint ? t('cloud.share.optionsWithHint', { hint: sharePassCodeHint(sharePassCodeConstraint) }) : t('cloud.share.options') })}</p>
        {serverGeneratedPassCode && onUpdatePassCode && <p>{t('cloud.share.generatedThenChangeable')}</p>}
        <div className="cloud-share-options">
          {alwaysGeneratedPassCode ? <label>{t('cloud.share.passCodeGeneratedByProvider')}</label> : serverGeneratedPassCode ? <div className="cloud-share-code-field">
            <span>{t('cloud.share.passCodeLabel')}</span>
            <label className="cloud-share-checkbox"><input ref={initialFocusRef} aria-label={t('cloud.share.autoGenerate')} type="checkbox" checked={generatePassCode} onChange={(event) => setGeneratePassCode(event.target.checked)} /><span>{t('cloud.share.autoGenerate')}</span></label>
          </div> : <label>{t(sharePassCodeConstraint?.required ? 'cloud.share.requiredPassCode' : 'cloud.share.optionalPassCode')}<input ref={initialFocusRef} aria-invalid={Boolean(passCodeError)} aria-label={t('cloud.share.passCodeLabel')} disabled={busy || generatePassCode} value={passCode} maxLength={sharePassCodeConstraint?.maxLength ?? 32} onChange={(event) => setPassCode(event.target.value)} /></label>}
          {sharePassCodeConstraint?.required && <label className="cloud-share-checkbox"><input type="checkbox" checked={generatePassCode} disabled={busy} onChange={(event) => setGeneratePassCode(event.target.checked)} />{t('cloud.share.autoGenerate')}</label>}
          <label>{t('cloud.share.expiration')}<select aria-label={t('cloud.share.expiration')} value={expiresInDays} onChange={(event) => setExpiresInDays(event.target.value)}>{expiryOptions.map(([value, label]) => <option key={value} value={value}>{label}</option>)}</select></label>
        </div>
        <button type="button" className="secondary" disabled={busy} onClick={onClose}>{t('dialog.cancel')}</button><button className="primary" disabled={busy || Boolean(passCodeError)} type="submit">{busy ? t('cloud.share.creating') : t('cloud.share.create')}</button>
      </form>}
    </section>
  </div>, document.body);
}

async function openCloudInformationLink(url: string) {
  const destination = new URL(url);
  if (destination.protocol !== 'https:') return;
  if (typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window) {
    const { openUrl } = await import('@tauri-apps/plugin-opener');
    await openUrl(destination);
    return;
  }
  window.open(destination, '_blank', 'noopener,noreferrer');
}

function CloudProviderInfo({ provider, label, officialUrl }: { provider: CloudProvider; label: string; officialUrl: string }) {
  const { t } = useTranslation();
  const externalLink = (url: string) => (event: ReactMouseEvent<HTMLAnchorElement>) => {
    event.preventDefault();
    void openCloudInformationLink(url);
  };

  return <section className="cloud-provider-info" role="region" aria-label={t('cloud.providerInfo', { name: label })}>
    <dl>
      <div><dt>{t('cloud.officialWebsite')}</dt><dd><a href={officialUrl} target="_blank" rel="noreferrer" aria-label={t('cloud.providerWebsite', { name: label })} onClick={externalLink(officialUrl)}>{officialUrl}</a></dd></div>
      <div><dt>{t('cloud.pluginAuthor')}</dt><dd><a href="https://versadown.aigutta.com" target="_blank" rel="noreferrer" aria-label={t('cloud.pluginAuthorName')} onClick={externalLink('https://versadown.aigutta.com')}>aigutta</a></dd></div>
      </dl>
      {provider === 'pikpak' && <p className="cloud-provider-info-notice">{t('cloud.info.pikpakAccess')}</p>}
      {provider === 'xunlei' && <p className="cloud-provider-info-notice">{t('cloud.info.xunleiSvip')}</p>}
      {provider === 'tianyi' && <p className="cloud-provider-info-notice">{t('cloud.info.tianyiPortal')} <a href="https://e.dlife.cn" target="_blank" rel="noreferrer" aria-label={t('cloud.info.tianyiLoginPortal')} onClick={externalLink('https://e.dlife.cn')}>e.dlife.cn</a> {t('cloud.info.tianyiDeviceLock')}</p>}
    </section>;
}

function CloudDestinationPicker({ action, provider, descriptor, client, excludedIds, onChoose, onOfficialWebAuthorizationRequired, onClose }: { action: DestinationAction; provider: CloudProvider; descriptor: ProviderDescriptor; client: CloudManagerClient; excludedIds: readonly string[]; onChoose(parentId: string): void; onOfficialWebAuthorizationRequired(error: unknown): void; onClose(): void }) {
  const { t } = useTranslation();
  const rootId = descriptor.rootDirectoryId;
  const [directory, setDirectory] = useState<CloudDirectory | null>(null);
  const [breadcrumbs, setBreadcrumbs] = useState<readonly Breadcrumb[]>([{ id: rootId, name: t('cloud.root') }]);
  const [state, setState] = useState<'loading' | 'ready' | 'error'>('loading');
  const [message, setMessage] = useState('');
  const actionLabel = action === 'copy' ? t('cloud.action.copy') : t('cloud.action.move');
  const current = breadcrumbs.at(-1) ?? { id: rootId, name: t('cloud.root') };
  const parent = breadcrumbs.at(-2);
  const dialogRef = useRef<HTMLElement>(null);
  const closeRef = useRef<HTMLButtonElement>(null);
  const requestSequence = useRef(0);
  useModalFocusTrap(dialogRef, closeRef, onClose);

  async function load(parentId: string, nextBreadcrumbs: readonly Breadcrumb[]) {
    const requestId = ++requestSequence.current;
    setBreadcrumbs(nextBreadcrumbs);
    setDirectory(null);
    setState('loading');
    setMessage('');
    try {
      const nextDirectory = await client.listCloudDirectory(provider, parentId);
      if (requestId !== requestSequence.current) return;
      setDirectory(nextDirectory);
      setState('ready');
    } catch (error) {
      if (requestId !== requestSequence.current) return;
      if (requiresOfficialWebAuthorization(error)) {
        onOfficialWebAuthorizationRequired(error);
        return;
      }
      setMessage(errorMessage(error, t('cloud.error.readDirectoryShort')));
      setState('error');
    }
  }

  useEffect(() => {
    void load(rootId, [{ id: rootId, name: t('cloud.root') }]);
    return () => { requestSequence.current += 1; };
  }, [client, provider]);
  const recycleBinId = descriptor.recycleBinDirectoryId;
  const folders = directory?.items.filter((item) => item.kind === 'folder' && !excludedIds.includes(item.id) && item.id !== recycleBinId) ?? [];
  return createPortal(<div className="cloud-confirm-backdrop">
    <section ref={dialogRef} className="cloud-destination-picker" role="dialog" aria-modal="true" aria-label={t('cloud.destination.title', { action: actionLabel })}>
      <header><div><h3>{t('cloud.destination.title', { action: actionLabel })}</h3><small>{t('cloud.currentDirectory', { name: current.name })}</small></div><button ref={closeRef} aria-label={t('cloud.destination.close', { action: actionLabel })} onClick={onClose}>×</button></header>
      <div className="cloud-destination-path">
        {parent && <button className="cloud-icon-button" aria-label={t('cloud.destination.backTo', { name: parent.name })} disabled={state === 'loading'} onClick={() => void load(parent.id, breadcrumbs.slice(0, -1))}><CloudActionIcon action="back" /></button>}
        <nav role="navigation" aria-label={t('cloud.destination.folder')} >{breadcrumbs.map((crumb, index) => <span key={`${crumb.id}-${index}`}><button disabled={state === 'loading' || index === breadcrumbs.length - 1} onClick={() => void load(crumb.id, breadcrumbs.slice(0, index + 1))}>{crumb.name}</button>{index < breadcrumbs.length - 1 && <i>›</i>}</span>)}</nav>
      </div>
      <div className="cloud-destination-list">
        {state === 'loading' && <div className="cloud-loading-state" role="status"><i /><span>{t('cloud.loadingDirectory')}</span></div>}
        {state === 'error' && <div className="cloud-state cloud-error" role="alert"><span>{message}</span><button className="secondary" onClick={() => void load(current.id, breadcrumbs)}>{t('common.retryAction')}</button></div>}
        {state === 'ready' && (folders.length ? folders.map((folder) => <button key={folder.id} aria-label={t('cloud.openNamed', { name: folder.name })} onClick={() => void load(folder.id, [...breadcrumbs, { id: folder.id, name: folder.name }])}><span><CloudFileIcon kind="folder" /></span><b>{folder.name}</b><i>›</i></button>) : <p>{t('cloud.noSubfolders')}</p>)}
      </div>
      <footer><button className="secondary" onClick={onClose}>{t('dialog.cancel')}</button><button className="primary" disabled={state !== 'ready'} onClick={() => onChoose(current.id)}>{t('cloud.destination.chooseHere', { action: actionLabel })}</button></footer>
    </section>
  </div>, document.body);
}

export function CloudManager({ provider, descriptor, client = unavailableClient, initialAccount = null, recoveryFailed = false, onDownloadTask, authorizationOnly = false, restoreOnMount = true, showSessionActions = true, workspaceVisible = true, privacyDownloadEnabled = false, authorizationCardDrag, onAccountAuthorized, onAccountLoggedOut }: CloudManagerProps) {
  const { t } = useTranslation();
  const label = cloudProviderLabel(provider, t);
  const rootId = descriptor.rootDirectoryId;
  const [activeAccount, setActiveAccount] = useState<CloudAccount | null>(initialAccount?.provider === provider ? initialAccount : null);
  const [accountCapacity, setAccountCapacity] = useState<CloudCapacityViewState>();
  const capacityControllerRef = useRef<CloudCapacityController | null>(null);
  if (!capacityControllerRef.current) capacityControllerRef.current = new CloudCapacityController();
  const capacityController = capacityControllerRef.current;
  const [directory, setDirectory] = useState<CloudDirectory | null>(null);
  const [directoryState, setDirectoryState] = useState<DirectoryState>('idle');
  const [breadcrumbs, setBreadcrumbs] = useState<readonly Breadcrumb[]>([{ id: rootId, name: t('cloud.root') }]);
  const [selectedIds, setSelectedIds] = useState<readonly string[]>([]);
  const [sort, setSort] = useState<CloudSort | null>(null);
  const [username, setUsername] = useState('');
  const [password, setPassword] = useState('');
  const [savedUsername, setSavedUsername] = useState<string | null>(null);
  const useSavedPassword = savedUsername === username.trim() && !password;
  const [passwordLoginBusy, setPasswordLoginBusy] = useState(false);
  const passwordLoginInFlight = useRef(false);
  const passwordFieldsEdited = useRef(false);
  const passwordInputVersion = useRef(0);
  const effectiveAuthorizationCapabilities = descriptor.authorization;
  const supportsPasswordLogin = effectiveAuthorizationCapabilities.password;
  const [authorizationStatus, setAuthorizationStatus] = useState(recoveryFailed ? expiredAuthorizationStatus : initialAuthorizationStatus);
  const [reauthorizationRequired, setReauthorizationRequired] = useState(false);
  const [openingOperationVerification, setOpeningOperationVerification] = useState(false);
  const [pendingAuthorizationName, setPendingAuthorizationName] = useState<string | null>(null);
  const [webAuthorization, setWebAuthorization] = useState<CloudAuthorization | null>(null);
  const [editor, setEditor] = useState<Editor>(null);
  const [editorValue, setEditorValue] = useState('');
  const [pendingTrash, setPendingTrash] = useState(false);
  const [pendingDownload, setPendingDownload] = useState<readonly CloudFile[] | null>(null);
  const [uploadPickerOpen, setUploadPickerOpen] = useState(false);
  const [pendingLogout, setPendingLogout] = useState(false);
  const [providerInfoOpen, setProviderInfoOpen] = useState(false);
  const [operation, setOperation] = useState<OperationState>(null);
  const [operationRunning, setOperationRunning] = useState(false);
  const [uploadResult, setUploadResult] = useState<OperationState>(null);
  const [uploadControlBusy, setUploadControlBusy] = useState(false);
  const [uploadControlError, setUploadControlError] = useState<string | null>(null);
  const uploadControlInFlight = useRef(false);
  const latestUploadProgress = useRef<CloudUploadProgress | null>(null);
  const [contextMenu, setContextMenu] = useState<CloudContextMenuState | null>(null);
  const [selectionMenuAnchor, setSelectionMenuAnchor] = useState<{ x: number; y: number } | null>(null);
  const [shareItems, setShareItems] = useState<readonly CloudFile[] | null>(null);
  const [createdShare, setCreatedShare] = useState<CloudShare | null>(null);
  const [destinationAction, setDestinationAction] = useState<DestinationAction | null>(null);
  const authorizationCompletionInFlight = useRef<number | null>(null);
  const webAuthorizationActive = useRef(false);
  const authorizationGeneration = useRef(0);
  const authorizationRegistered = useRef(false);
  const authorizationWake = useRef<(() => void) | null>(null);
  const authorizationWakePending = useRef(false);
  const operationInFlight = useRef(false);
  const pendingAuthorizationAction = useRef<PendingAuthorizationAction | null>(null);
  const selectionAnchor = useRef<string | null>(null);
  const selectAllCheckbox = useRef<HTMLInputElement>(null);
  const selectionMenuTrigger = useRef<HTMLButtonElement>(null);
  const fileListRef = useRef<HTMLDivElement>(null);

  const sortedItems = sortedCloudFiles(directory?.items ?? [], sort);
  const selected = sortedItems.filter((item) => selectedIds.includes(item.id));
  const currentDirectoryId = directory?.id ?? breadcrumbs.at(-1)?.id ?? rootId;
  const recycleBinId = descriptor.recycleBinDirectoryId;
  const isRecycleBin = currentDirectoryId === recycleBinId;
  const isMyShares = currentDirectoryId === MY_SHARES_DIRECTORY_ID;
  const isVirtualRecycleBinItem = (item: CloudFile | undefined) => item?.id === recycleBinId;
  const isVirtualMySharesItem = (item: CloudFile | undefined) => item?.id === MY_SHARES_DIRECTORY_ID;
  const isVirtualDirectoryItem = (item: CloudFile | undefined) => isVirtualRecycleBinItem(item) || isVirtualMySharesItem(item);
  const selectedVirtualRecycleBin = selected.some(isVirtualRecycleBinItem);
  const selectedVirtualMyShares = selected.some(isVirtualMySharesItem);
  const selectableItems = sortedItems.filter((item) => !isVirtualDirectoryItem(item));
  const operationBusy = operationRunning || operation?.kind === 'working';
  const passwordSession = activeAccount?.authMethod === 'password';
  const authorizedLabel = passwordSession ? t('cloud.loggedIn') : t('cloud.authorized');
  const logoutLabel = passwordSession ? t('cloud.logout') : t('cloud.revokeAuthorization');
  const allSelected = Boolean(selectableItems.length) && selected.length === selectableItems.length;

  useLayoutEffect(() => {
    const list = fileListRef.current;
    if (!list) return;
    const syncScrollbarGutter = () => {
      list.style.setProperty('--cloud-scrollbar-gutter-width', `${Math.max(0, list.offsetWidth - list.clientWidth)}px`);
    };
    syncScrollbarGutter();
    if (typeof ResizeObserver === 'undefined') return;
    const observer = new ResizeObserver(syncScrollbarGutter);
    observer.observe(list);
    return () => observer.disconnect();
  }, [directoryState]);

  function queueAuthorizationContinuation(name: string, resume: () => Promise<void>, message: string, showOperation = true, expectedAccountId = activeAccount?.id) {
    if (!expectedAccountId) return;
    pendingAuthorizationAction.current = { accountId: expectedAccountId, name, resume, showOperation };
    setPendingAuthorizationName(name);
    setReauthorizationRequired(true);
    setAuthorizationStatus(message);
    if (showOperation) setOperation({ kind: 'error', message });
  }

  function needsInPlaceWebAuthorization(error: unknown) {
    return requiresOfficialWebAuthorization(error)
      || (isCredentialExpired(error) && effectiveAuthorizationCapabilities.web);
  }

  function showOperationVerification(error: unknown) {
    // This is not a failed login. Never queue an automatic replay of a mutation
    // when the provider is waiting for a separate interactive challenge.
    pendingAuthorizationAction.current = null;
    setPendingAuthorizationName(null);
    setReauthorizationRequired(false);
    setOperation({ kind: 'error', message: requiresBaiduOperationVerification(error) ? baiduVerificationText().required : errorMessage(error, t('cloud.verification.pikpak.required')), operationVerification: true });
  }

  async function openOperationVerification() {
    if (!activeAccount || !client.openCloudOperationVerification || openingOperationVerification) return;
    setOpeningOperationVerification(true);
    try {
      if (provider === 'baidu') await client.openCloudOperationVerification(provider, activeAccount.id, document.documentElement.lang);
      else await client.openCloudOperationVerification(provider, activeAccount.id);
      setOperation({ kind: 'error', operationVerification: true, verificationOpened: true, message: provider === 'baidu' ? baiduVerificationText().opened : t('cloud.verification.pikpak.opened') });
    } catch (error) {
      setOperation({ kind: 'error', operationVerification: true, message: errorMessage(error, t('cloud.verification.openFailed')) });
    } finally {
      setOpeningOperationVerification(false);
    }
  }

  useEffect(() => {
    if (selectAllCheckbox.current) selectAllCheckbox.current.indeterminate = selected.length > 0 && !allSelected;
  }, [allSelected, selected.length]);

  useEffect(() => {
    setSelectionMenuAnchor(null);
  }, [selectedIds]);

  useEffect(() => {
    if (workspaceVisible) return;
    // Cached provider workspaces stay mounted. Discard settled notices when
    // leaving one, but keep an active upload and its controls until it finishes.
    if (!operationInFlight.current) {
      setOperation(null);
      setUploadResult(null);
    }
    setContextMenu(null);
    setSelectionMenuAnchor(null);
    setDestinationAction(null);
    setProviderInfoOpen(false);
    setUploadPickerOpen(false);
    setPendingDownload(null);
    setPendingTrash(false);
    setShareItems(null);
    setCreatedShare(null);
    setEditor(null);
  }, [workspaceVisible]);

  const previousInitialAccount = useRef(initialAccount);
  useEffect(() => {
    if (initialAccount === previousInitialAccount.current || operationInFlight.current) return;
    previousInitialAccount.current = initialAccount;
    if (!initialAccount || initialAccount === activeAccount || initialAccount.provider !== provider || initialAccount.id !== activeAccount?.id) return;
    // A renewed session can arrive from the management card while this file
    // workspace is cached. Consume its pending action through the same guarded
    // path as authorization completed in this workspace.
    void acceptAccount(initialAccount).catch(error => {
      setOperation({ kind: 'error', message: errorMessage(error, t('cloud.operation.recoveryFailed')) });
    });
  }, [initialAccount, operationBusy]);

  useEffect(() => {
    if (!supportsPasswordLogin) {
      setUsername('');
      setPassword('');
      return;
    }
    let active = true;
    client.getSavedCloudCredentials?.(provider).then((credentials) => {
      if (!active || passwordFieldsEdited.current || !credentials) return;
      setUsername(credentials.username);
      setSavedUsername(credentials.hasSavedPassword ? credentials.username : null);
    }).catch(() => undefined);
    return () => { active = false; };
  }, [client, provider, supportsPasswordLogin, activeAccount?.id]);

  useEffect(() => {
    if (recoveryFailed && !activeAccount) setAuthorizationStatus(expiredAuthorizationStatus);
  }, [activeAccount, recoveryFailed]);

  useEffect(() => {
    const account = activeAccount;
    if (!authorizationOnly || !account) {
      setAccountCapacity(undefined);
      return;
    }
    let active = true;
    let unlisten: (() => void) | undefined;
    const publish = (state: CloudCapacityViewState) => { if (active) setAccountCapacity(state); };
    const load = () => client.getCloudAccountCapacity!(provider);
    const cached = capacityController.read(provider, account.id);
    setAccountCapacity(cached ?? { loading: true, refreshFailed: false });
    if (client.getCloudAccountCapacity) {
      void capacityController.refresh(provider, account.id, load, publish);
      void client.subscribeCloudCapacityInvalidated?.(provider, event => {
        capacityController.invalidate(event, provider, account.id, load, publish);
      }).then(stop => {
        if (active) unlisten = stop;
        else stop();
      }).catch(() => undefined);
    } else {
      publish({ loading: false, refreshFailed: true });
    }
    return () => {
      active = false;
      unlisten?.();
      capacityController.clear(provider, account.id);
    };
  }, [activeAccount?.id, authorizationOnly, capacityController, client, provider]);

  useEffect(() => () => capacityController.dispose(), [capacityController]);

  async function loadDirectory(account: CloudAccount, parentId: string, nextBreadcrumbs: readonly Breadcrumb[]) {
    setDirectoryState('loading');
    setSelectedIds([]);
    setContextMenu(null);
    selectionAnchor.current = null;
    try {
      const next = parentId === MY_SHARES_DIRECTORY_ID
        ? { id: MY_SHARES_DIRECTORY_ID, items: (await client.listCloudShares(provider)).map((share) => ({
          id: `__versa_down_share__:${share.id}`,
          parentId: MY_SHARES_DIRECTORY_ID,
          name: share.name,
          kind: share.kind,
          modifiedAt: share.createdAt,
          shareId: share.id,
          shareUrl: share.url,
          sharePassCode: share.passCode,
        })) }
        : await client.listCloudDirectory(provider, parentId);
      const recycleBinId = descriptor.recycleBinDirectoryId;
      const rootItems = parentId === rootId
        ? [
          ...next.items,
      ...(next.items.some((item) => item.id === recycleBinId) ? [] : [{ id: recycleBinId, parentId: rootId, name: t('cloud.recycleBin'), kind: 'folder' as const }]),
      ...(next.items.some((item) => item.id === MY_SHARES_DIRECTORY_ID) ? [] : [{ id: MY_SHARES_DIRECTORY_ID, parentId: rootId, name: t('cloud.myShares'), kind: 'folder' as const }]),
        ]
        : next.items;
      const items = rootItems;
      const visibleDirectory = { ...next, items };
      setDirectory(visibleDirectory);
      setBreadcrumbs(nextBreadcrumbs);
      setDirectoryState(items.length ? 'ready' : 'empty');
      return visibleDirectory;
    } catch (error) {
      if (requiresPikpakOperationVerification(error)) {
        showOperationVerification(error);
        setDirectoryState('error');
        setAuthorizationStatus(errorMessage(error, t('cloud.authorization.completeOfficialVerification')));
      } else if (needsInPlaceWebAuthorization(error)) {
        const detail = errorMessage(error, '');
        const message = t('cloud.authorization.directoryRequiresWeb', { detail: detail ? ` ${detail}` : '' });
        queueAuthorizationContinuation(t('cloud.action.loadDirectory'), async () => { await loadDirectory(account, parentId, nextBreadcrumbs); }, message, Boolean(directory), account.id);
        setDirectoryState(directory ? (directory.items.length ? 'ready' : 'empty') : 'error');
        return null;
      }
      setDirectory(null);
      setDirectoryState(isCredentialExpired(error) ? 'expired' : 'error');
      setAuthorizationStatus(errorMessage(error, t('cloud.error.readDirectory')));
      return null;
    }
  }

  function navigateToDirectory(parentId: string, nextBreadcrumbs: readonly Breadcrumb[]) {
    if (!activeAccount || operationBusy) return;
    // Operation-driven refreshes call loadDirectory directly, so they retain
    // the result until the user actually leaves the current directory.
    setOperation(null);
    setUploadResult(null);
    void loadDirectory(activeAccount, parentId, nextBreadcrumbs);
  }

  async function completeOperationVerification() {
    if (provider !== 'baidu' || !activeAccount || !client.completeCloudOperationVerification || openingOperationVerification) return;
    setOpeningOperationVerification(true);
    try {
      const account = await client.completeCloudOperationVerification(provider, activeAccount.id);
      if (account.id !== activeAccount.id || account.provider !== provider) throw { code: 'account_mismatch', message: baiduVerificationText().accountMismatch };
      setActiveAccount(account);
      onAccountAuthorized?.(account);
      setOperation({ kind: 'success', message: baiduVerificationText().completed });
    } catch (error) {
      const message = typeof error === 'object' && error !== null && 'code' in error && error.code === 'account_mismatch'
        ? baiduVerificationText().accountMismatch
        : baiduVerificationText().completeFailed;
      setOperation({ kind: 'error', operationVerification: true, verificationOpened: true, message });
    } finally {
      setOpeningOperationVerification(false);
    }
  }

  async function refreshUploadedDirectory(
    account: CloudAccount,
    parentId: string,
    nextBreadcrumbs: readonly Breadcrumb[],
    uploaded: readonly CloudFile[]
  ) {
    // Directory uploads also return descendants. Only their outermost results
    // determine where to confirm the upload; children stay in their new folder.
    const uploadedIds = new Set(uploaded.map((file) => file.id));
    const topLevel = uploaded.filter((file) => !uploadedIds.has(file.parentId));
    const destinations = [...new Set(topLevel.map((file) => file.parentId))];
    let destinationName: string | undefined;
    if (destinations.length === 1 && destinations[0] !== parentId) {
      const original = await loadDirectory(account, parentId, nextBreadcrumbs);
      if (!original) return { loaded: false, confirmed: false };
      const targetId = destinations[0];
      const target = original.items.find((file) => file.id === targetId && file.kind === 'folder');
      destinationName = target?.name ?? (targetId === rootId ? t('cloud.root') : t('cloud.upload.actualDestination'));
      nextBreadcrumbs = target
        ? [...nextBreadcrumbs, { id: targetId, name: target.name }]
        : targetId === rootId
          ? [{ id: rootId, name: t('cloud.root') }]
          : [{ id: rootId, name: t('cloud.root') }, { id: targetId, name: destinationName }];
      parentId = targetId;
    }
    for (const delay of DIRECTORY_INDEX_CONFIRMATION_DELAYS_MS) {
      if (delay > 0) await new Promise<void>((resolve) => window.setTimeout(resolve, delay));
      const refreshed = await loadDirectory(account, parentId, nextBreadcrumbs);
      if (!refreshed) return { loaded: false, confirmed: false };
      // Mixed file/directory batches may have more than one actual destination.
      // Confirm each one without navigating repeatedly between directories.
      const listings = new Map([[parentId, refreshed]]);
      let lookupFailed = false;
      for (const targetId of destinations) {
        if (listings.has(targetId)) continue;
        try {
          listings.set(targetId, await client.listCloudDirectory(provider, targetId));
        } catch {
          lookupFailed = true;
        }
      }
      if (!lookupFailed && topLevel.every((file) => listings.get(file.parentId)?.items.some((item) => item.id === file.id))) {
        return { loaded: true, confirmed: true, destinationName };
      }
    }
    return { loaded: true, confirmed: false, destinationName };
  }

  async function refreshDirectoryUntil(
    account: CloudAccount,
    parentId: string,
    nextBreadcrumbs: readonly Breadcrumb[],
    confirmation: DirectoryRefreshConfirmation
  ) {
    for (const delay of DIRECTORY_INDEX_CONFIRMATION_DELAYS_MS) {
      if (delay > 0) await new Promise<void>((resolve) => window.setTimeout(resolve, delay));
      const refreshed = await loadDirectory(account, parentId, nextBreadcrumbs);
      if (!refreshed) return { loaded: false, confirmed: false };
      if (confirmation(refreshed)) return { loaded: true, confirmed: true };
    }
    return { loaded: true, confirmed: false };
  }

  useEffect(() => {
    if (!authorizationOnly && activeAccount && directoryState === 'idle') void loadDirectory(activeAccount, rootId, [{ id: rootId, name: t('cloud.root') }]);
  }, [activeAccount, authorizationOnly, directoryState]);

  useEffect(() => {
    if (authorizationOnly || !restoreOnMount) return;
    let active = true;
    client.getCloudAccount(provider).then((account) => {
      if (!active) return;
      ensureProviderAccount(account);
      setActiveAccount(account);
    }).catch((error) => {
      if (active && !isUnauthorized(error)) setAuthorizationStatus(errorMessage(error, t('cloud.authorization.restoreSessionFailed')));
    });
    return () => { active = false; };
  }, [authorizationOnly, client, provider, restoreOnMount]);

  function ensureProviderAccount(account: CloudAccount): CloudAccount {
    if (account.provider !== provider) {
      throw { code: 'invalid_response', message: t('cloud.authorization.providerMismatch') };
    }
    return account;
  }

  async function acceptAccount(account: CloudAccount) {
    ensureProviderAccount(account);
    const pending = pendingAuthorizationAction.current;
    const sameAccount = !pending
      || pending.accountId === account.id
      || (provider === 'tianyi' && (pending.accountId === 'tianyi-web' || account.id === 'tianyi-web'));
    if (pending && !sameAccount) {
      const message = t('cloud.authorization.accountMismatch');
      webAuthorizationActive.current = false;
      setWebAuthorization(null);
      setReauthorizationRequired(true);
      setAuthorizationStatus(message);
      if (pending.showOperation) setOperation({ kind: 'error', message });
      return false;
    }
    setActiveAccount(account);
    setWebAuthorization(null);
    setReauthorizationRequired(false);
    pendingAuthorizationAction.current = null;
    setPendingAuthorizationName(null);
    if (pending) setOperation(null);
    onAccountAuthorized?.(account);
    if (pending) await pending.resume();
    else if (!authorizationOnly) await loadDirectory(account, rootId, [{ id: rootId, name: t('cloud.root') }]);
    return true;
  }

  async function addAccount(authorize: () => Promise<CloudAccount>) {
    setAuthorizationStatus(t('cloud.authorization.waitingResult'));
    try {
      await acceptAccount(await authorize());
    } catch (error) {
      if (requiresOfficialWebAuthorization(error)) setReauthorizationRequired(true);
      setAuthorizationStatus(errorMessage(error, t('cloud.authorization.failedRetry')));
    }
  }

  async function submitPasswordLogin(event: FormEvent) {
    event.preventDefault();
    if (!supportsPasswordLogin || passwordLoginInFlight.current) return;
    if (!username.trim() || (!password && !useSavedPassword)) {
      setAuthorizationStatus(t('cloud.authorization.enterCredentials'));
      return;
    }
    const credentials: CloudCredentials = useSavedPassword
      ? { username: username.trim(), useSavedPassword: true }
      : { username: username.trim(), password };
    const submittedInputVersion = passwordInputVersion.current;
    passwordLoginInFlight.current = true;
    setPasswordLoginBusy(true);
    try {
      await addAccount(() => client.loginCloudWithPassword(provider, credentials));
    } finally {
      // Submitted plaintext is released from component state after the attempt.
      if (credentials.password && passwordInputVersion.current === submittedInputVersion) {
        setPassword('');
        const saved = await client.getSavedCloudCredentials?.(provider).catch(() => null);
        if (passwordInputVersion.current === submittedInputVersion) {
          setSavedUsername(saved?.hasSavedPassword ? saved.username : null);
        }
      }
      passwordLoginInFlight.current = false;
      setPasswordLoginBusy(false);
    }
  }

  async function beginWebAuthorization() {
    if (webAuthorizationActive.current) return;
    const generation = ++authorizationGeneration.current;
    webAuthorizationActive.current = true;
    authorizationRegistered.current = false;
    authorizationWakePending.current = false;
    setWebAuthorization({ provider, windowLabel: '', instructions: '' });
    setAuthorizationStatus(t('cloud.authorization.preparingWindow'));
    try {
      const authorization = await client.beginCloudAuthorization(provider);
      if (!webAuthorizationActive.current || generation !== authorizationGeneration.current) return;
      authorizationRegistered.current = true;
      setWebAuthorization(authorization);
      setAuthorizationStatus(authorization.instructions);
      if (authorizationWakePending.current) authorizationWake.current?.();
    } catch (error) {
      if (generation !== authorizationGeneration.current) return;
      webAuthorizationActive.current = false;
      setWebAuthorization(null);
      setAuthorizationStatus(errorMessage(error, t('cloud.authorization.openWindowFailed')));
    }
  }

  async function completeWebAuthorization() {
    const generation = authorizationGeneration.current;
    if (!webAuthorizationActive.current || !authorizationRegistered.current || authorizationCompletionInFlight.current === generation) return;
    authorizationCompletionInFlight.current = generation;
    try {
      const account = await client.completeCloudAuthorization(provider);
      if (!webAuthorizationActive.current || generation !== authorizationGeneration.current) return;
      ensureProviderAccount(account);
      const accepted = await acceptAccount(account);
      if (accepted) webAuthorizationActive.current = false;
    } catch (error) {
      if (!webAuthorizationActive.current || generation !== authorizationGeneration.current) return;
      if (isAuthorizationPending(error)) {
        setAuthorizationStatus(t('cloud.authorization.waitingLogin'));
        return;
      }
      const message = errorMessage(error, t('cloud.authorization.webNotCompleted'));
      setAuthorizationStatus(message);
      if (effectiveAuthorizationCapabilities?.passwordLoginMode === 'officialWebView' && isTerminalWebAuthorizationError(error)) {
        // A rejected verification must not automatically start another captcha.
        // Keep the account and let the user explicitly reopen authorization.
        setWebAuthorization(null);
        webAuthorizationActive.current = false;
        if (activeAccount) {
          setReauthorizationRequired(true);
          setOperation({ kind: 'error', message });
        }
      }
      if (requiresOfficialWebAuthorization(error)) {
        webAuthorizationActive.current = false;
        setWebAuthorization(null);
      }
    } finally {
      if (authorizationCompletionInFlight.current === generation) authorizationCompletionInFlight.current = null;
    }
  }

  const completeWebAuthorizationRef = useRef(completeWebAuthorization);
  completeWebAuthorizationRef.current = completeWebAuthorization;
  const acceptAccountRef = useRef(acceptAccount);
  acceptAccountRef.current = acceptAccount;
  const waitingForWebAuthorization = webAuthorization !== null;
  useEffect(() => {
    if (!waitingForWebAuthorization) return;
    let cancelled = false;
    let unlisten: (() => void) | undefined;
    let timer: number | undefined;
    const generation = authorizationGeneration.current;
    let querying = false;
    const current = () => !cancelled && webAuthorizationActive.current && generation === authorizationGeneration.current;
    const wake = async () => {
      if (!current() || querying) return;
      if (!authorizationRegistered.current) { authorizationWakePending.current = true; return; }
      querying = true;
      try {
        const account = await client.getCompletedCloudAuthorization?.(provider);
        if (!current() || account?.provider !== provider) return;
        const accepted = await acceptAccountRef.current(account);
        if (accepted && timer !== undefined) window.clearTimeout(timer);
      } catch { /* Lost wakeups are covered by the watchdog. */ }
      finally { querying = false; }
    };
    authorizationWake.current = () => { void wake(); };
    void client.subscribeCloudAuthorization?.(provider, event => {
      if (event.provider === provider && event.state === 'Ready') void wake();
    }).then(stop => { if (cancelled) stop(); else unlisten = stop; }).catch(() => { /* The watchdog also covers unavailable event delivery. */ });
    const poll = () => {
      timer = window.setTimeout(async () => {
        if (!current()) return;
        await completeWebAuthorizationRef.current();
        if (current()) poll();
      }, authorizationWatchdogMs);
    };
    poll();
    return () => {
      cancelled = true;
      if (generation === authorizationGeneration.current) {
        webAuthorizationActive.current = false;
        authorizationWake.current = null;
      }
      unlisten?.();
      if (timer !== undefined) window.clearTimeout(timer);
    };
  }, [client, provider, waitingForWebAuthorization]);

  async function runOperation(name: string, action: () => Promise<unknown>, refresh = true, retryUploadFailures?: (failures: CloudUploadOutcome['failures']) => () => Promise<void>, confirmDirectoryRefresh?: DirectoryRefreshConfirmation) {
    if (!activeAccount || operationInFlight.current) return;
    const operationLabel = t(({ '下载': 'download.action.download', '上传': 'cloud.action.upload', '新建目录': 'cloud.action.newFolder', '重命名': 'cloud.action.rename', '恢复': 'cloud.action.restore', '永久删除': 'cloud.action.permanentDelete', '删除': 'download.action.delete', '创建分享': 'cloud.share.create', '取消分享': 'cloud.action.cancelShare', '移动': 'cloud.action.move', '复制': 'cloud.action.copy', '退出登录': 'cloud.logout', '退出授权': 'cloud.revokeAuthorization' } as Record<string, Parameters<typeof t>[0]>)[name] ?? 'cloud.action.inProgress');
    operationInFlight.current = true;
    setOperationRunning(true);
    setOperation({ kind: 'working', message: t('cloud.operation.working', { name: operationLabel }) });
    try {
      const result = await action();
      if (name.startsWith('上传') && isCloudUploadOutcome(result)) {
        if (!result.cancelled && result.uploaded.length === 0 && result.failures.length === 0) {
          setOperation(null);
          return;
        }
        const listedFailures = result.failures.slice(0, 3).map(failure => `${failure.itemName}：${failure.message}`).join('；');
        const remainingFailures = result.failures.length > 3 ? t('cloud.upload.remainingFailures', { count: result.failures.length - 3 }) : '';
        const report: NonNullable<OperationState> = result.cancelled
          ? { kind: 'cancelled', message: t('cloud.upload.cancelledWithResults', { count: result.uploaded.length, failures: result.failures.some(failure => failure.code !== 'upload_cancelled') ? result.failures.filter(failure => failure.code !== 'upload_cancelled').map(failure => `${failure.itemName}: ${failure.message}`).slice(0, 3).join('; ') : '' }) }
          : result.failures.length
          ? { kind: 'error', message: t('cloud.upload.results', { name: operationLabel, succeeded: result.uploaded.length, failed: result.failures.length, failures: listedFailures, remaining: remainingFailures }) }
          : { kind: 'success', message: t('cloud.upload.itemsCompleted', { count: result.uploaded.length }) };
        // The upload result is authoritative even when the follow-up listing fails.
        setUploadResult(report);
        setOperation(report);
        const directoryRefresh = refresh
          ? await refreshUploadedDirectory(activeAccount, currentDirectoryId, breadcrumbs, result.uploaded)
          : { loaded: true, confirmed: true, destinationName: undefined };
        if (!directoryRefresh.loaded) return;
        const destinationMessage = directoryRefresh.destinationName
          ? t('cloud.upload.destination', { name: directoryRefresh.destinationName }) : '';
        const finalReport = {
          ...report,
          message: `${report.message}${destinationMessage}${directoryRefresh.confirmed ? '' : t('cloud.directoryNotSynced')}`,
        };
        setUploadResult(finalReport);
        if (result.cancelled) {
          setOperation(finalReport);
          return;
        }
        if (result.failures.length > 0) {
          const first = result.failures.find(failure => ['web_auth_required', 'credentials_expired', 'session_renewed_retry_operation'].includes(failure.code)) ?? result.failures[0];
          if (first.code === 'web_auth_required' || (first.code === 'credentials_expired' && effectiveAuthorizationCapabilities.web)) {
            const authorizationFailures = result.failures.filter(failure => failure.code === 'web_auth_required' || (failure.code === 'credentials_expired' && effectiveAuthorizationCapabilities.web));
            queueAuthorizationContinuation(name, retryUploadFailures?.(authorizationFailures) ?? (() => runOperation(name, action, refresh, retryUploadFailures, confirmDirectoryRefresh)), t('cloud.webVerificationRequired', { name: operationLabel, detail: first.message ? ` ${first.message}` : '' }));
            return;
          }
          if (first.code === 'credentials_expired') {
            setDirectory(null);
            setDirectoryState('expired');
            setAuthorizationStatus(first.message || t('cloud.credentialsExpiredWithPeriod'));
            return;
          }
          if (first.code === 'session_renewed_retry_operation') {
            setOperation({ kind: 'error', message: t('cloud.upload.recoveredRetryFailures', { succeeded: result.uploaded.length, failed: result.failures.length }) });
            return;
          }
          setOperation(finalReport);
          return;
        }
        setOperation(finalReport);
        setEditor(null);
        setEditorValue('');
        return;
      }
      const directoryRefresh = refresh
        ? confirmDirectoryRefresh
          ? await refreshDirectoryUntil(activeAccount, currentDirectoryId, breadcrumbs, confirmDirectoryRefresh)
          : { loaded: Boolean(await loadDirectory(activeAccount, currentDirectoryId, breadcrumbs)), confirmed: true }
        : { loaded: true, confirmed: true };
      if (!directoryRefresh.loaded) return;
      const message = name === '下载'
        ? t('cloud.download.submitted')
        : name === '上传' && Array.isArray(result)
          ? t('cloud.upload.filesCompleted', { count: result.length })
          : t('cloud.operation.succeeded', { name: operationLabel });
      setOperation({ kind: 'success', message: `${message}${directoryRefresh.confirmed ? '' : t('cloud.directoryNotSynced')}` });
      setEditor(null);
      setEditorValue('');
    } catch (error) {
      if (requiresPikpakOperationVerification(error) || requiresBaiduOperationVerification(error)) {
        showOperationVerification(error);
      } else if (needsInPlaceWebAuthorization(error)) {
        // Keep the existing account and directory alive while the official
        // browser proves the requested action. `cloud_begin_authorization`
        // only opens the WebView; the saved session is replaced later, after
        // a successful `cloud_complete_authorization` validation.
        const detail = errorMessage(error, '');
        queueAuthorizationContinuation(name, () => runOperation(name, action, refresh, retryUploadFailures, confirmDirectoryRefresh), t('cloud.webVerificationRequired', { name: operationLabel, detail: detail ? ` ${detail}` : '' }));
      } else if (isCredentialExpired(error)) {
        setDirectory(null);
        setDirectoryState('expired');
        setAuthorizationStatus(errorMessage(error, t('cloud.credentialsExpiredWithPeriod')));
      } else if (isSessionRenewedRetryOperation(error)) {
        setOperation({ kind: 'error', message: t('cloud.operation.recoveredRetry', { name: operationLabel }) });
      } else {
        setOperation({ kind: 'error', message: t('cloud.operation.failed', { name: operationLabel, detail: errorMessage(error, t('common.retry')) }) });
      }
    } finally {
      operationInFlight.current = false;
      setOperationRunning(false);
      setOperation(current => current?.kind === 'working'
        ? { kind: 'error', message: t('cloud.operation.listRefreshFailed', { name: operationLabel }) }
        : current);
    }
  }

  function openEditor(next: Exclude<Editor, null>) {
    if (isRecycleBin || isMyShares || selectedVirtualRecycleBin || selectedVirtualMyShares) return;
    setContextMenu(null);
    setEditor(next);
    setEditorValue(next === 'rename' ? selected[0]?.name ?? '' : '');
  }

  function toggleSelection(item: CloudFile) {
    if (isVirtualDirectoryItem(item)) return;
    const { id: itemId } = item;
    setSelectedIds((current) => current.includes(itemId) ? current.filter((id) => id !== itemId) : [...current, itemId]);
  }

  function selectRow(event: ReactMouseEvent, item: CloudFile, index: number) {
    if (isVirtualDirectoryItem(item)) {
      setSelectedIds([]);
      selectionAnchor.current = null;
      return;
    }
    const anchorIndex = selectionAnchor.current === null ? -1 : sortedItems.findIndex((entry) => entry.id === selectionAnchor.current);
    if (event.shiftKey && anchorIndex >= 0) {
      const from = Math.min(anchorIndex, index);
      const to = Math.max(anchorIndex, index);
      const range = sortedItems.slice(from, to + 1).filter((entry) => !isVirtualDirectoryItem(entry)).map((entry) => entry.id);
      setSelectedIds(event.ctrlKey || event.metaKey ? (current) => Array.from(new Set([...current.filter((id) => id !== recycleBinId && id !== MY_SHARES_DIRECTORY_ID), ...range])) : range);
    } else if (event.ctrlKey || event.metaKey) {
      toggleSelection(item);
    } else {
      setSelectedIds([item.id]);
    }
    selectionAnchor.current = item.id;
  }

  function openFolder(item: CloudFile) {
    if (!activeAccount || isRecycleBin || isMyShares || item.kind !== 'folder' || operationBusy) return;
    navigateToDirectory(item.id, [...breadcrumbs, { id: item.id, name: item.name }]);
  }

  function changeSort(column: CloudSortColumn) {
    setSort((current) => current?.column === column
      ? { column, direction: current.direction === 'ascending' ? 'descending' : 'ascending' }
      : { column, direction: 'ascending' });
  }

  function sortDescription(column: CloudSortColumn) {
    if (sort?.column !== column) return t('cloud.sort.unsorted');
    return t('cloud.sort.currentDirection', { direction: t(sort.direction === 'ascending' ? 'cloud.sort.ascending' : 'cloud.sort.descending') });
  }

  function submitDownload(items: readonly CloudFile[], privateDownload?: boolean) {
    void runOperation('下载', async () => {
      const directoryFailures: CloudDirectoryDownloadFailure[] = [];
      let queuedDirectoryFiles = 0;
      await Promise.all(items.map(async (item) => {
        if (item.kind === 'folder') {
          const outcome = privateDownload === undefined
            ? await client.downloadCloudDirectory(provider, item.id, item.name)
            : await client.downloadCloudDirectory(provider, item.id, item.name, privateDownload);
          onDownloadTask?.(outcome.task);
          directoryFailures.push(...outcome.failures);
          queuedDirectoryFiles += outcome.queuedCount;
          return;
        }
        const task = privateDownload === undefined
          ? await client.downloadCloudFile(provider, item.id, item.name)
          : await client.downloadCloudFile(provider, item.id, item.name, privateDownload);
        onDownloadTask?.(task);
      }));
      if (directoryFailures.length) {
        const details = directoryFailures.slice(0, 5).map((failure) => `${failure.relativePath}：${failure.message}`).join('；');
        const remaining = directoryFailures.length > 5 ? `；另有 ${directoryFailures.length - 5} 项失败` : '';
        throw new Error(`目录下载已入队 ${queuedDirectoryFiles} 个文件；${details}${remaining}`);
      }
    }, false);
  }

  function downloadSelection() {
    if (!selected.length || selectedVirtualRecycleBin || selectedVirtualMyShares || isRecycleBin || isMyShares) return;
    if (privacyDownloadEnabled) {
      setPendingDownload(selected.slice());
      return;
    }
    submitDownload(selected);
  }

  function confirmPendingDownload(privateDownload: boolean) {
    const items = pendingDownload;
    setPendingDownload(null);
    if (items?.length) submitDownload(items, privateDownload);
  }

  function uploadFiles() {
    if (isRecycleBin || isMyShares || operationBusy) return;
    setUploadPickerOpen(true);
  }

  async function uploadSelectedFiles(paths: string[]) {
    await runOperation('上传', async () => {
      setUploadResult(null);
      setUploadControlError(null);
      latestUploadProgress.current = null;
      let acceptingProgress = true;
      try {
        return await client.uploadCloudFiles!(provider, currentDirectoryId, paths, progress => {
          if (!acceptingProgress || progress.provider !== provider) return;
          latestUploadProgress.current = progress;
          if (['completed', 'failed', 'cancelled', 'cancelling'].includes(progress.phase)) setUploadControlError(null);
          setOperation(current => current?.kind === 'working' ? { ...current, uploadProgress: progress } : current);
        });
      } finally {
        acceptingProgress = false;
        latestUploadProgress.current = null;
      }
    }, true, (failures) => {
      const indexed = failures
        .map((failure) => failure.inputIndex)
        .filter((index): index is number => typeof index === 'number' && Number.isInteger(index) && index >= 0 && index < paths.length);
      const retryPaths = indexed.length === failures.length ? indexed.map((index) => paths[index]) : paths;
      return () => uploadSelectedFiles(retryPaths);
    });
  }

  function refreshDirectory() {
    if (activeAccount) void loadDirectory(activeAccount, currentDirectoryId, breadcrumbs);
  }

  function openShare(items = selected) {
    if (!items.length || isRecycleBin || isMyShares || selectedVirtualRecycleBin || selectedVirtualMyShares) return;
    setCreatedShare(null);
    setShareItems(items.slice());
  }

  function openSharePassCodeEditor(item = selected[0]) {
    if (!isMyShares || !descriptor.capabilities.shareUpdatePassCode || !item?.shareId || !item.shareUrl || operationBusy) return;
    setShareItems([item]);
    setCreatedShare({ id: item.shareId, url: item.shareUrl, name: item.name, kind: item.kind, sourceFileIds: [], passCode: item.sharePassCode });
  }

  async function updateSharePassCode(passCode: string) {
    const share = createdShare;
    if (!share || !descriptor.capabilities.shareUpdatePassCode || !client.updateCloudSharePassCode) throw new Error(t('cloud.share.passCodeChangeUnsupported'));
    await client.updateCloudSharePassCode(provider, share.id, passCode);
    setCreatedShare((current) => current?.id === share.id ? { ...current, passCode } : current);
    setDirectory((current) => current ? { ...current, items: current.items.map(item => item.shareId === share.id ? { ...item, sharePassCode: passCode } : item) } : current);
  }

  function submitCloudShare(options: CloudShareOptions) {
    const items = shareItems;
    if (!items?.length) return;
    void runOperation('创建分享', async () => {
      const share = await client.createCloudShare(provider, items.map((item) => item.id), options);
      setCreatedShare(share);
      return share;
    }, false);
  }

  function cancelCloudShares(shareIds: readonly string[]) {
    const targets = [...new Set(shareIds)];
    if (!targets.length) return;
    void runOperation('取消分享', () => client.cancelCloudShares(provider, targets), true);
  }

  function copyShareLink(value: string, passCode?: string) {
    void copyCloudShareLink(cloudShareCopyUrl(value, passCode)).then(() => setOperation({ kind: 'success', message: t('cloud.share.linkCopied') })).catch((error) => setOperation({ kind: 'error', message: t('cloud.share.copyLinkFailed', { detail: errorMessage(error, t('common.retry')) }) }));
  }

  function runContextAction(action: CloudMenuAction) {
    const menu = contextMenu;
    setContextMenu(null);
    if (menu?.kind === 'selection' && (isVirtualRecycleBinItem(menu.item) || isVirtualMySharesItem(menu.item)) && action !== 'open') return;
    if (action === 'open' && menu?.kind === 'selection') openFolder(menu.item);
    if (action === 'download') downloadSelection();
    if (action === 'restore' && !selectedVirtualRecycleBin) void runOperation('恢复', () => client.restoreCloudFiles(provider, selectedIds.slice()));
    if (action === 'rename' && !isRecycleBin) openEditor('rename');
    if (action === 'move' && !isRecycleBin) setDestinationAction('move');
    if (action === 'copy' && !isRecycleBin) setDestinationAction('copy');
    if (action === 'share') openShare();
    if (action === 'updateSharePassCode' && selected.length === 1) openSharePassCodeEditor();
    if (action === 'copyShareLink' && menu?.kind === 'selection' && menu.item.shareUrl) copyShareLink(menu.item.shareUrl, menu.item.sharePassCode);
    if (action === 'cancelShare' && menu?.kind === 'selection') {
      const shareIds = selected.map((item) => item.shareId).filter((id): id is string => Boolean(id));
      cancelCloudShares(shareIds);
    }
    if (action === 'delete') setPendingTrash(true);
    if (action === 'create' && !isRecycleBin) openEditor('create');
    if (action === 'upload') uploadFiles();
    if (action === 'refresh') refreshDirectory();
  }

  function openSelectionOverflowMenu() {
    const bounds = selectionMenuTrigger.current?.getBoundingClientRect();
    if (!bounds) return;
    setSelectionMenuAnchor({ x: bounds.right, y: bounds.bottom + 6 });
  }

  function runSelectionOverflowAction(action: CloudHeaderMenuAction) {
    setSelectionMenuAnchor(null);
    selectionMenuTrigger.current?.focus();
    if (selectedVirtualRecycleBin || selectedVirtualMyShares) return;
    if (action === 'rename' && !isRecycleBin) openEditor('rename');
    if (action === 'move' && !isRecycleBin) setDestinationAction('move');
    if (action === 'copy' && !isRecycleBin) setDestinationAction('copy');
    if (action === 'share') openShare();
  }

  function handleFileListKeyDown(event: ReactKeyboardEvent<HTMLDivElement>) {
    if ((event.target as HTMLElement).closest('.cloud-sort-button')) return;
    if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === 'a') {
      event.preventDefault();
      setSelectedIds(selectableItems.map((item) => item.id));
      return;
    }
    if (event.key === 'Escape') {
      setContextMenu(null);
      setEditor(null);
      return;
    }
    if (event.key === 'Enter' && !isRecycleBin && !isMyShares && selected.length === 1 && selected[0].kind === 'folder') {
      event.preventDefault();
      openFolder(selected[0]);
      return;
    }
    if (event.key === 'F2' && !isRecycleBin && !isMyShares && !selectedVirtualRecycleBin && !selectedVirtualMyShares && selected.length === 1) {
      event.preventDefault();
      openEditor('rename');
      return;
    }
    if (event.key === 'Delete' && selected.length && !selectedVirtualRecycleBin && !selectedVirtualMyShares && !isMyShares) {
      event.preventDefault();
      setPendingTrash(true);
    }
  }

  function submitEditor(event: FormEvent) {
    event.preventDefault();
    if (!activeAccount || isRecycleBin || isMyShares || selectedVirtualRecycleBin || selectedVirtualMyShares || !editorValue.trim()) return;
    if (editor === 'create') void runOperation('新建目录', () => client.createCloudDirectory(provider, currentDirectoryId, editorValue.trim()).then(() => undefined));
    if (editor === 'rename' && selected[0]) void runOperation('重命名', () => client.renameCloudFile(provider, selected[0].id, editorValue.trim()));
  }

  const displayedAuthorizationStatus = authorizationStatus === initialAuthorizationStatus
    ? t('cloud.authorization.notAuthorized')
    : authorizationStatus === expiredAuthorizationStatus
      ? t('cloud.authorization.expired')
      : authorizationStatus;
  function authorizationView(expired = false) {
    const showAuthorizationFeedback = authorizationStatus !== initialAuthorizationStatus;
    const showAuthorizationError = expired || authorizationStatus === expiredAuthorizationStatus;
    return <section className={`cloud-manager cloud-authorization cloud-provider-${provider}${authorizationCardDrag?.dragging ? ' cloud-provider-dragging' : ''}${authorizationCardDrag?.dropTarget ? ' cloud-provider-drop-target' : ''}`} data-cloud-provider={authorizationCardDrag ? provider : undefined} aria-label={t('cloud.accountAuthorization', { name: label })}>
      <header className="cloud-heading cloud-authorization-drag-handle" onPointerDown={authorizationCardDrag?.onPointerDown}><h2>{t('cloud.providerDrive', { name: label })}</h2><button type="button" onPointerDown={(event) => event.stopPropagation()} className="cloud-provider-info-button" aria-label={providerInfoOpen ? t('cloud.closeProviderInfo', { name: label }) : t('cloud.viewProviderInfo', { name: label })} aria-expanded={providerInfoOpen} onClick={() => setProviderInfoOpen((open) => !open)}>{providerInfoOpen ? '×' : <CloudActionIcon action="details" />}</button></header>
      {providerInfoOpen ? <CloudProviderInfo provider={provider} label={label} officialUrl={descriptor.authorizationUrl} /> : <><form className="cloud-login-form" onSubmit={submitPasswordLogin}>
        {supportsPasswordLogin && <>
          <input aria-label={t('cloud.username')} placeholder={t('cloud.username')} autoComplete="username" value={username} onChange={(event) => { passwordFieldsEdited.current = true; passwordInputVersion.current += 1; setUsername(event.target.value); }} />
          <input aria-label={t('cloud.password')} placeholder={useSavedPassword ? "******" : t('cloud.password')} type="password" autoComplete="current-password" value={password} onChange={(event) => { passwordFieldsEdited.current = true; passwordInputVersion.current += 1; setSavedUsername(null); setPassword(event.target.value); }} />
          <button className="primary" type="submit" disabled={passwordLoginBusy}>{passwordLoginBusy ? t('cloud.autoLogin') : t('cloud.login')}</button>
        </>}
        {effectiveAuthorizationCapabilities.web && <button type="button" className="secondary" disabled={passwordLoginBusy} onClick={() => void beginWebAuthorization()}>{t('cloud.webAuthorization')}</button>}
        {webAuthorization && <button type="button" className="secondary" onClick={() => void completeWebAuthorization()}>{t('cloud.authorizationCompleted')}</button>}
      </form>
      {showAuthorizationFeedback && <p className={`cloud-authorization-feedback${showAuthorizationError ? ' error' : ''}`} role="status">{expired && <b>{t('cloud.credentialsExpired')}</b>}<span>{displayedAuthorizationStatus}</span></p>}</>}
    </section>;
  }

  function authorizationContinuationButton() {
    if (operation?.operationVerification && client.openCloudOperationVerification) {
      return <><button className="secondary cloud-authorization-continuation" disabled={openingOperationVerification || operationBusy} onClick={() => void openOperationVerification()}>{provider === 'baidu' ? baiduVerificationText().open : t('cloud.verification.pikpak.open')}</button>{provider === 'baidu' && operation.verificationOpened && client.completeCloudOperationVerification && <button className="secondary cloud-authorization-continuation" disabled={openingOperationVerification || operationBusy} onClick={() => void completeOperationVerification()}>{baiduVerificationText().complete}</button>}</>;
    }
    if (!pendingAuthorizationName || !effectiveAuthorizationCapabilities.web) return null;
    return <button className="secondary cloud-authorization-continuation" disabled={operationBusy || webAuthorization !== null} onClick={() => void beginWebAuthorization()}>{t('cloud.authorization.openAndContinue', { name: label, action: pendingAuthorizationName })}</button>;
  }

  function sessionActions() {
    return <div className="cloud-session-actions"><button className="cloud-auth-state" disabled>{authorizedLabel}</button>{reauthorizationRequired && effectiveAuthorizationCapabilities?.web && <button className="secondary" disabled={operationBusy} onClick={() => void beginWebAuthorization()}>{t('cloud.reauthorizeWeb')}</button>}<button className="secondary" disabled={operationBusy} onClick={() => setPendingLogout(true)}>{logoutLabel}</button></div>;
  }

  function logoutConfirmation() {
    return pendingLogout && <div className="cloud-confirm-backdrop"><section className="cloud-trash-confirm" role="alertdialog" aria-modal="true" aria-label={t('cloud.confirmAction', { action: logoutLabel })}><h3>{t('cloud.confirmAction', { action: logoutLabel })}</h3><p>{t('cloud.logoutWarning')}</p><button className="secondary" onClick={() => setPendingLogout(false)}>{t('dialog.cancel')}</button><button className="danger-button" onClick={() => { setPendingLogout(false); void runOperation(logoutLabel, async () => {
      await client.logoutCloud(provider);
      passwordFieldsEdited.current = false;
      passwordInputVersion.current += 1;
      setUsername('');
      setSavedUsername(null);
      setPassword('');
      setActiveAccount(null);
      setDirectory(null);
      setDirectoryState('idle');
      setAuthorizationStatus(initialAuthorizationStatus);
      onAccountLoggedOut?.(provider);
    }, false); }}>{t('cloud.confirmAction', { action: logoutLabel })}</button></section></div>;
  }

  function accountManagementView() {
    const capacityText = accountCapacity?.value
      ? formatCloudCapacity(accountCapacity.value.capacity)
      : accountCapacity?.loading || !accountCapacity ? t('cloud.capacityLoading') : t('cloud.capacityUnavailable');
    return <section className={`cloud-manager cloud-authorization cloud-connected-account cloud-provider-${provider}${authorizationCardDrag?.dragging ? ' cloud-provider-dragging' : ''}${authorizationCardDrag?.dropTarget ? ' cloud-provider-drop-target' : ''}`} data-cloud-provider={authorizationCardDrag ? provider : undefined} aria-label={t('cloud.accountAuthorization', { name: label })}>
      <header className="cloud-heading cloud-authorization-drag-handle" onPointerDown={authorizationCardDrag?.onPointerDown}><h2>{t('cloud.providerDrive', { name: label })} <span className="cloud-account-name">· {activeAccount?.displayName}</span></h2><button type="button" onPointerDown={(event) => event.stopPropagation()} className="cloud-provider-info-button" aria-label={providerInfoOpen ? t('cloud.closeProviderInfo', { name: label }) : t('cloud.viewProviderInfo', { name: label })} aria-expanded={providerInfoOpen} onClick={() => setProviderInfoOpen((open) => !open)}>{providerInfoOpen ? '×' : <CloudActionIcon action="details" />}</button></header>
      {!providerInfoOpen && <p className="cloud-account-capacity" aria-live="polite">{capacityText}{accountCapacity?.value && accountCapacity.refreshFailed && <span> · {t('cloud.capacityRefreshFailed')}</span>}</p>}
      {providerInfoOpen ? <CloudProviderInfo provider={provider} label={label} officialUrl={descriptor.authorizationUrl} /> : <>{sessionActions()}{logoutConfirmation()}{operationStatus()}</>}
    </section>;
  }

  async function controlUpload(action: CloudUploadAction) {
    const progress = latestUploadProgress.current;
    if (!progress?.uploadId || !client.controlCloudUpload || uploadControlInFlight.current
      || ['completed', 'failed', 'cancelled', 'cancelling'].includes(progress.phase)) return;
    uploadControlInFlight.current = true;
    setUploadControlBusy(true);
    setUploadControlError(null);
    try {
      await client.controlCloudUpload(provider, progress.uploadId, action);
      const latest = latestUploadProgress.current;
      if (!latest || latest.uploadId !== progress.uploadId || ['completed', 'failed', 'cancelled', 'cancelling'].includes(latest.phase)) return;
      const next: CloudUploadProgress = { ...latest, phase: action === 'pause' ? 'paused' : action === 'cancel' ? 'cancelling' : 'uploading' };
      latestUploadProgress.current = next;
      setOperation(current => current?.uploadProgress?.uploadId === progress.uploadId ? { ...current, uploadProgress: next } : current);
    } catch (error) {
      const latest = latestUploadProgress.current;
      if (latest?.uploadId === progress.uploadId && !['completed', 'failed', 'cancelled', 'cancelling'].includes(latest.phase)) setUploadControlError(errorMessage(error, t('cloud.upload.controlFailed')));
    } finally {
      uploadControlInFlight.current = false;
      setUploadControlBusy(false);
    }
  }

  function operationStatus() {
    const notice = operation ?? uploadResult;
    if (!notice) return null;
    const progress = operation?.uploadProgress;
    // `runOperation('上传', …)` posts this localized text until the first progress event.
    const uploadWorkingMessage = t('cloud.operation.working', { name: t('cloud.action.upload') });
    const phaseLabels = { preparing: t('cloud.upload.preparing'), uploading: t('cloud.upload.uploading'), finalizing: t('cloud.upload.finalizing'), paused: t('cloud.upload.paused'), cancelling: t('cloud.upload.cancelling'), cancelled: t('cloud.upload.cancelled'), completed: t('cloud.upload.completed'), failed: t('cloud.upload.failed') };
    const controllable = progress && !['completed', 'failed', 'cancelled'].includes(progress.phase) && client.controlCloudUpload && progress.uploadId;
    return <div className={`cloud-operation-status ${notice.kind}`} role={notice.kind === 'error' ? 'alert' : 'status'}>
      <div className="cloud-upload-status-content">
        {!progress && <span>{notice.message === uploadWorkingMessage ? t('cloud.upload.preparing') : notice.message}</span>}
        {uploadResult && uploadResult.message !== notice.message && <p>{uploadResult.message}</p>}
        {progress && <>
          <div>{phaseLabels[progress.phase]}{progress.currentFiles.length > 0 && `：${progress.currentFiles.join('、')}`}</div>
          <div className="cloud-upload-stats"><span>{progress.progress.toFixed(1).replace(/\.0$/, '')}% · {readableSize(progress.uploadedBytes)} / {readableSize(progress.totalBytes)} · {readableSize(Math.round(progress.speedBytesPerSecond))}/s</span><span>{t('cloud.upload.stats', { completed: progress.completedFiles, total: progress.totalFiles, failed: progress.failedItems })}</span></div>
          <div className="cloud-upload-track-row">
            <div className="progress-track" role="progressbar" aria-label={t('cloud.upload.totalProgress')} aria-valuemin={0} aria-valuemax={100} aria-valuenow={progress.progress}><i style={{ width: `${progress.progress}%` }} /></div>
            {controllable && <>
              <button type="button" className="cloud-upload-control-button" aria-label={progress.phase === 'paused' ? t('cloud.upload.resume') : t('cloud.upload.pause')} title={progress.phase === 'paused' ? t('cloud.upload.resume') : t('cloud.upload.pause')} disabled={uploadControlBusy || progress.phase === 'cancelling'} onClick={() => void controlUpload(progress.phase === 'paused' ? 'resume' : 'pause')}><CloudActionIcon action={progress.phase === 'paused' ? 'resume' : 'pause'} /></button>
              <button type="button" className="cloud-upload-control-button" aria-label={progress.phase === 'cancelling' ? t('cloud.upload.cancelling') : t('cloud.upload.cancel')} title={progress.phase === 'cancelling' ? t('cloud.upload.cancelling') : t('cloud.upload.cancel')} disabled={uploadControlBusy || progress.phase === 'cancelling'} onClick={() => void controlUpload('cancel')}><CloudActionIcon action="cancel" /></button>
            </>}
          </div>
          {uploadControlError && <p className="cloud-error" role="alert">{uploadControlError}</p>}
        </>}
      </div>
      {notice.kind === 'error' && authorizationContinuationButton()}
    </div>;
  }

  if (authorizationOnly && activeAccount) return accountManagementView();
  if (authorizationOnly || !activeAccount) return authorizationView();
  if (directoryState === 'expired' && !showSessionActions) return <section className="cloud-manager" aria-label={t('cloud.fileManager', { name: label })}><header className="cloud-heading"><h2>{t('cloud.providerDrive', { name: label })}</h2></header>{operationStatus()}<div className="cloud-state cloud-error" role="alert"><span>{t('cloud.credentialsExpired')}.</span>{authorizationContinuationButton()}</div></section>;
  if (directoryState === 'expired') return authorizationView(true);

  const parent = breadcrumbs.at(-2);
  const canNavigateToParent = Boolean(parent);
  const navigateToParent = () => {
    if (!parent || operationBusy) return;
    navigateToDirectory(parent.id, breadcrumbs.slice(0, -1));
  };
  return <section className="cloud-manager cloud-file-manager" aria-label={t('cloud.fileManager', { name: label })}>
    {showSessionActions && <div className="cloud-file-sessionbar">
      {showSessionActions && sessionActions()}
    </div>}

    <div className="cloud-filebar">
      <button className="cloud-icon-button cloud-refresh-button" aria-label={t('cloud.action.refresh')} title={t('cloud.action.refresh')} disabled={operationBusy} onClick={refreshDirectory}><CloudActionIcon action="refresh" /></button>
      <div className="cloud-pathbar">
        <div className="cloud-breadcrumbs" role="navigation" aria-label={t('cloud.currentDirectory', { name: breadcrumbs.at(-1)?.name ?? '' })}>{breadcrumbs.map((crumb, index) => <span key={`${crumb.id}-${index}`}><button title={crumb.name} aria-current={index === breadcrumbs.length - 1 ? 'page' : undefined} disabled={index === breadcrumbs.length - 1 || operationBusy} onClick={() => navigateToDirectory(crumb.id, breadcrumbs.slice(0, index + 1))}>{crumb.name}</button>{index < breadcrumbs.length - 1 && <i aria-hidden="true">›</i>}</span>)}</div>
      </div>
      <div className="cloud-toolbar cloud-toolbar--single-line" aria-label={t('cloud.fileActions')}>
        {!isRecycleBin && !isMyShares && <button aria-label={t('cloud.action.upload')} disabled={operationBusy} onClick={uploadFiles}><CloudActionIcon action="upload" />{t('cloud.action.upload')}</button>}
        {!isRecycleBin && !isMyShares && <button aria-label={t('cloud.action.newFolder')} disabled={operationBusy} onClick={() => openEditor('create')}><CloudActionIcon action="create" />{t('cloud.action.newFolder')}</button>}
        {isMyShares && descriptor.capabilities.shareUpdatePassCode && selected.length === 1 && <button disabled={operationBusy} onClick={() => openSharePassCodeEditor()}><CloudActionIcon action="updateSharePassCode" />{t('cloud.share.changePassCode')}</button>}
      </div>
    </div>

    {editor && <form className="cloud-operation-editor" onSubmit={submitEditor} onKeyDown={(event) => { if (event.key === 'Escape') { event.preventDefault(); setEditor(null); } }}><label>{editor === 'create' ? t('cloud.folderName') : t('cloud.newName')}<input aria-label={editor === 'create' ? t('cloud.folderName') : t('cloud.newName')} value={editorValue} onChange={(event) => setEditorValue(event.target.value)} autoFocus /></label><button className="primary" type="submit" disabled={operationBusy}>{editor === 'create' ? t('cloud.confirmCreateFolder') : t('cloud.confirmRename')}</button><button type="button" className="secondary" disabled={operationBusy} onClick={() => setEditor(null)}>{t('dialog.cancel')}</button></form>}
    {pendingTrash && <div className="cloud-confirm-backdrop"><section className="cloud-trash-confirm" role="alertdialog" aria-modal="true" aria-label={isRecycleBin ? t('cloud.confirmPermanentlyDelete') : t('cloud.confirmMoveToRecycleBin')}><h3>{isRecycleBin ? t('cloud.confirmPermanentlyDelete') : t('cloud.confirmMoveToRecycleBin')}</h3><p>{isRecycleBin ? t('cloud.permanentDeleteWarning', { count: selected.length }) : t('cloud.moveToRecycleBinWarning', { count: selected.length })}</p><button className="secondary" onClick={() => setPendingTrash(false)}>{t('dialog.cancel')}</button><button className="danger-button" onClick={() => { setPendingTrash(false); void runOperation(isRecycleBin ? t('cloud.action.permanentDelete') : t('download.action.delete'), () => isRecycleBin ? client.deleteCloudFiles(provider, selectedIds.slice()) : client.trashCloudFiles(provider, selectedIds.slice())); }}>{isRecycleBin ? t('cloud.action.permanentDelete') : t('cloud.moveToRecycleBin')}</button></section></div>}
    {pendingDownload && <div className="cloud-confirm-backdrop"><section className="cloud-trash-confirm" role="alertdialog" aria-modal="true" aria-label={t('download.confirmContent')}><h3>{t('download.confirmContent')}</h3><p>{t('cloud.downloadSelectionPrompt', { count: pendingDownload.length })}</p><button className="secondary" onClick={() => setPendingDownload(null)}>{t('dialog.cancel')}</button><button className="primary" onClick={() => confirmPendingDownload(false)}>{t('download.downloadNow')}</button><button className="secondary" onClick={() => confirmPendingDownload(true)}>{t('download.privateDownload')}</button></section></div>}
    {showSessionActions && logoutConfirmation()}
    {operationStatus()}
    {!showSessionActions && reauthorizationRequired && effectiveAuthorizationCapabilities?.web && <button className="secondary" disabled={operationBusy} onClick={() => void beginWebAuthorization()}>{t('cloud.reauthorizeWeb')}</button>}
    {directoryState === 'loading' && <div className="cloud-loading-state" role="status"><i /><span>{t('cloud.loadingFiles')}</span></div>}
    {directoryState === 'error' && <div className="cloud-state cloud-error" role="alert"><b>{t('cloud.loadingFailed')}</b><span>{displayedAuthorizationStatus}</span>{authorizationContinuationButton()}<button className="secondary" onClick={refreshDirectory}>{t('common.retryAction')}</button></div>}
    {(directoryState === 'ready' || directoryState === 'empty') && <div
      className="cloud-list"
      ref={fileListRef}
      role="tree"
      aria-label={t('cloud.driveFiles', { name: label })}
      tabIndex={0}
      onKeyDown={handleFileListKeyDown}
      onContextMenu={(event) => { if (!(event.target as HTMLElement).closest('[data-cloud-file-row]')) { event.preventDefault(); setSelectedIds([]); setContextMenu({ kind: 'background', x: event.clientX, y: event.clientY }); } }}
    >
      <div className="cloud-row cloud-columns"><span className="cloud-check-cell"><input ref={selectAllCheckbox} type="checkbox" aria-label={t('cloud.selectCurrentDirectory')} checked={allSelected} onChange={(event) => setSelectedIds(event.target.checked ? selectableItems.map((item) => item.id) : [])} /></span><span className="cloud-name-column">{selected.length > 0 ? <span className="cloud-header-selection" role="toolbar" aria-label={t('cloud.selectedFileActions', { count: selected.length })}><b>{t('cloud.itemsSelected', { count: selected.length })}</b>{selectedVirtualRecycleBin || selectedVirtualMyShares ? <span className="cloud-selection-hint">{t('cloud.specialFolderOpenOnly')}</span> : isRecycleBin ? <span className="cloud-selection-actions"><button disabled={operationBusy} onClick={() => void runOperation(t('cloud.action.restore'), () => client.restoreCloudFiles(provider, selectedIds.slice()))}><CloudActionIcon action="restore" />{t('cloud.action.restore')}</button><button className="danger" disabled={operationBusy} onClick={() => setPendingTrash(true)}><CloudActionIcon action="delete" />{t('cloud.action.permanentDelete')}</button></span> : isMyShares ? <span className="cloud-selection-actions"><button disabled={operationBusy || !selected[0]?.shareUrl} onClick={() => selected[0]?.shareUrl && copyShareLink(selected[0].shareUrl, selected[0].sharePassCode)}><CloudActionIcon action="copyShareLink" />{t('cloud.action.copyLink')}</button><button className="danger" disabled={operationBusy} onClick={() => cancelCloudShares(selected.map((item) => item.shareId).filter((id): id is string => Boolean(id)))}><CloudActionIcon action="cancelShare" />{t('cloud.action.cancelShare')}</button></span> : <span className="cloud-selection-actions"><button disabled={operationBusy} onClick={downloadSelection}><CloudActionIcon action="download" />{t('download.action.download')}</button><button className="danger" disabled={operationBusy} onClick={() => setPendingTrash(true)}><CloudActionIcon action="delete" />{t('download.action.delete')}</button><button ref={selectionMenuTrigger} className="cloud-selection-more" aria-label={t('cloud.moreFileActions')} aria-haspopup="menu" aria-expanded={Boolean(selectionMenuAnchor)} disabled={operationBusy} onClick={openSelectionOverflowMenu}>•••</button></span>}<button className="cloud-clear-selection" aria-label={t('cloud.clearSelection')} title={t('cloud.clearSelection')} onClick={() => setSelectedIds([])}><CloudActionIcon action="clear" /></button></span> : <button className="cloud-sort-button" type="button" aria-label={t('cloud.sortByName')} aria-description={sortDescription('name')} aria-pressed={sort?.column === 'name'} onClick={() => changeSort('name')}>{t('download.sort.name')}<span aria-hidden="true">{sort?.column === 'name' ? sort.direction === 'ascending' ? '↑' : '↓' : '↕'}</span></button>}</span><span><button className="cloud-sort-button" type="button" aria-label={t('cloud.sortBySize')} aria-description={sortDescription('size')} aria-pressed={sort?.column === 'size'} onClick={() => changeSort('size')}>{t('download.sort.size')}<span aria-hidden="true">{sort?.column === 'size' ? sort.direction === 'ascending' ? '↑' : '↓' : '↕'}</span></button></span><span><button className="cloud-sort-button" type="button" aria-label={t('cloud.sortByModifiedAt')} aria-description={sortDescription('modifiedAt')} aria-pressed={sort?.column === 'modifiedAt'} onClick={() => changeSort('modifiedAt')}>{t('download.sort.time')}<span aria-hidden="true">{sort?.column === 'modifiedAt' ? sort.direction === 'ascending' ? '↑' : '↓' : '↕'}</span></button></span></div>
      {canNavigateToParent && <div className="cloud-row cloud-file-row cloud-parent-directory-row" role="treeitem" aria-label={t('cloud.parentDirectory')} tabIndex={0} onClick={navigateToParent} onKeyDown={(event) => { if (event.key === 'Enter' || event.key === ' ') { event.preventDefault(); event.stopPropagation(); navigateToParent(); } }} onContextMenu={(event) => { event.preventDefault(); event.stopPropagation(); }}>
        <span className="cloud-check-cell"><input type="checkbox" aria-label={t('cloud.selectNamed', { name: t('cloud.parentDirectory') })} disabled /></span>
        <span className="cloud-file-name"><i className="folder"><CloudFileIcon kind="folder" /></i><b title={t('cloud.parentDirectory')}>{t('cloud.parentDirectory')}</b></span>
        <span>—</span><span>—</span>
      </div>}
      {directoryState === 'empty' ? <div className="cloud-empty"><CloudFileIcon kind="folder" /><b>{isRecycleBin ? t('cloud.empty.recycleBin.title') : isMyShares ? t('cloud.empty.myShares.title') : t('cloud.empty.directory.title')}</b><span>{isRecycleBin ? t('cloud.empty.recycleBin.body') : isMyShares ? t('cloud.empty.myShares.body') : t('cloud.empty.directory.body')}</span></div> : sortedItems.map((item, index) => {
        const isSelected = selectedIds.includes(item.id);
        const isVirtualDirectory = isVirtualDirectoryItem(item);
        const iconKind: CloudFileIconKind = isVirtualRecycleBinItem(item) ? 'recycle-bin' : isVirtualMySharesItem(item) ? 'my-shares' : item.kind;
        return <div
          className={`cloud-row cloud-file-row ${isSelected ? 'selected' : ''}`}
          data-cloud-virtual-folder={isVirtualMySharesItem(item) ? 'my-shares' : undefined}
          data-cloud-file-row
          role="treeitem"
          aria-selected={isSelected}
          key={item.id}
          onClick={(event) => selectRow(event, item, index)}
          onDoubleClick={(event) => { event.preventDefault(); openFolder(item); }}
          onContextMenu={(event) => { event.preventDefault(); event.stopPropagation(); if (isVirtualDirectory) { setSelectedIds([]); selectionAnchor.current = null; } else { if (!isSelected) setSelectedIds([item.id]); selectionAnchor.current = item.id; } setContextMenu({ kind: 'selection', item, x: event.clientX, y: event.clientY }); }}
        >
          <span className="cloud-check-cell"><input type="checkbox" aria-label={t('cloud.selectNamed', { name: item.name })} disabled={isVirtualDirectory} checked={isSelected} onClick={(event) => event.stopPropagation()} onChange={() => toggleSelection(item)} /></span>
          <span className="cloud-file-name"><i className={iconKind}><CloudFileIcon kind={iconKind} /></i><b title={item.name}>{item.name}</b></span>
          <span>{item.kind === 'folder' ? '—' : readableSize(item.size)}</span>
          <span>{formatCloudModifiedAt(item.modifiedAt)}</span>
        </div>;
      })}
    </div>}
    {uploadPickerOpen && <LocalUploadPicker listEntries={client.listLocalUploadEntries ?? unavailableLocalUploadListing} onCancel={() => setUploadPickerOpen(false)} onConfirm={paths => { setUploadPickerOpen(false); void uploadSelectedFiles(paths); }} />}
    {contextMenu && <CloudContextMenu state={contextMenu} selectedCount={selected.length} recycleBin={isRecycleBin} virtualDirectory={contextMenu.kind === 'selection' && isVirtualDirectoryItem(contextMenu.item)} myShares={isMyShares} canUpdateSharePassCode={Boolean(descriptor.capabilities.shareUpdatePassCode)} onClose={() => setContextMenu(null)} onAction={runContextAction} />}
    {selectionMenuAnchor && <CloudSelectionOverflowMenu anchor={selectionMenuAnchor} singleSelection={selected.length === 1} recycleBin={isRecycleBin} disabled={Boolean(operationBusy)} onClose={() => setSelectionMenuAnchor(null)} onAction={runSelectionOverflowAction} />}
    {shareItems && <CloudShareDialog provider={provider} items={shareItems} share={createdShare} busy={operationBusy} sharePassCodeConstraint={descriptor.sharePassCodeConstraint} onUpdatePassCode={descriptor.capabilities.shareUpdatePassCode ? updateSharePassCode : undefined} onCreate={submitCloudShare} onCopy={copyShareLink} onClose={() => { setShareItems(null); setCreatedShare(null); }} />}
    {destinationAction && <CloudDestinationPicker action={destinationAction} descriptor={descriptor} provider={provider} client={client} excludedIds={selectedIds} onOfficialWebAuthorizationRequired={(error) => { const action = destinationAction; setDestinationAction(null); queueAuthorizationContinuation(`选择${action === 'copy' ? '复制' : '移动'}目标`, async () => { setDestinationAction(action); }, errorMessage(error, `读取${action === 'copy' ? '复制' : '移动'}目标目录需要网页验证。`)); }} onClose={() => setDestinationAction(null)} onChoose={(targetParentId) => { const action = destinationAction; const sourceIds = selectedIds.slice(); setDestinationAction(null); if (action === 'move') void runOperation('移动', () => client.moveCloudFiles(provider, sourceIds, targetParentId), true, undefined, (directory) => sourceIds.every((id) => !directory.items.some((item) => item.id === id))); else void runOperation('复制', () => client.copyCloudFiles(provider, sourceIds, targetParentId)); }} />}
  </section>;
}

function isCloudUploadOutcome(value: unknown): value is CloudUploadOutcome {
  return typeof value === 'object' && value !== null
    && Array.isArray((value as CloudUploadOutcome).uploaded)
    && Array.isArray((value as CloudUploadOutcome).failures);
}

function unavailableLocalUploadListing(): Promise<LocalUploadListing> {
  return Promise.reject(new Error('本地文件选择器不可用，请使用最新版本。'));
}
