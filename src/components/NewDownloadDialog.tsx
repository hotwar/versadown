import { FormEvent, ReactNode, useEffect, useLayoutEffect, useRef, useState } from 'react';
import { isTauriRuntime, listPersistedTasks } from '../lib/api';
import { formatBytes } from '../lib/downloadEvents';
import { ConfirmResource, ParseDownloadResult, ScanDownloadProgress, ScanDownloadResult } from '../lib/types';
import { ResourceFileRow } from './ResourceFileRow';
import { VideoFormatCard } from './VideoFormatCard';
import { canExpandDownloadDraft, DownloadDraftItem, DraftSubmissionFailure, useDownloadDraftBatch } from './useDownloadDraftBatch';
import { t, useTranslation } from '../i18n';

export type NewDownloadDialogPlacement = 'launcher' | 'modal' | 'header';

export interface NewDownloadDialogProps {
  open: boolean;
  placement?: NewDownloadDialogPlacement;
  onParse(input: string, accessCode?: string, probeVideo?: boolean): void | Promise<ParseDownloadResult | void>;
  onScan?(input: string, onProgress?: (progress: ScanDownloadProgress) => void): Promise<ScanDownloadResult[]>;
  onClose(): void;
  onOpenTaskList?(): void | Promise<void>;
  initialInput?: string;
  onDiscardPrepared?(reference: string): Promise<void>;
  onSubmitBatch?(items: DownloadDraftItem[], destinationDirectory?: string, privateDownload?: boolean, postProcessingNote?: string): void | Promise<void | DraftSubmissionFailure[]>;
  downloadDirectory?: string;
  browseDirectory?(currentDirectory: string): Promise<string | null | undefined>;
  privacyEnabled?: boolean;
}

// Parse errors currently cross IPC as text. All providers use the same action
// policy: unavailable shares close; explicit transient/auth/code errors retry.
function isUnavailableShare(message: string | null): boolean {
  if (!message) return false;
  // A generic suggestion to check a code is not a confirmed code error.
  if (/超时|请求(?:过于)?频繁|限流|(?:请|需要)(?:先|重新)?(?:登录|授权)|(?:登录|授权|令牌)(?:已)?(?:过期|失效|无效)|(?:提取码|访问码|分享码|密码)(?:(?:校验|验证)?(?:错误|失败|无效)|不正确|缺失)/.test(message)) return false;
  return /分享(?:链接|地址|文件|内容|的文件)?(?:已(?:经)?(?:被)?|被)?(?:取消|删除|失效|过期|屏蔽|封禁|不存在|无效|不可用|无权访问|禁止访问|拒绝访问)/.test(message)
    || /(?:已|已经)(?:被)?(?:取消|停止)(?:了)?分享/.test(message)
    || /分享[^。；\n]*封禁[^。；\n]*(?:无法访问|受限)/.test(message);
}

type DownloadBatch = { entries: ScanDownloadResult[]; index: number };

function directAutoStarts(entry: ScanDownloadResult, text: string): boolean {
  return entry.valid
    && entry.input === text
    && entry.kind === 'direct'
    && !entry.canProbeVideo;
}

export function NewDownloadDialog(props: NewDownloadDialogProps) {
  return props.onSubmitBatch ? <BatchNewDownloadDialog {...props} /> : <LegacyNewDownloadDialog {...props} />;
}

function batchItemTitle(item: DownloadDraftItem): string {
  const title = item.result?.confirmation?.title ?? item.result?.task?.name ?? item.result?.duplicate?.task.name;
  if (title && !isGenericBatchTitle(title)) return title;
  return batchResourceTitle(batchItemResources(item)) ?? title ?? item.input;
}

function batchItemResources(item: DownloadDraftItem): readonly ConfirmResource[] {
  if (item.result?.confirmation) return item.result.confirmation.resources;
  const task = item.result?.duplicate?.task ?? item.result?.task;
  return task?.files?.map((file) => ({
    id: String(file.index),
    index: file.index,
    name: file.path,
    size: `${file.length} B`,
    selected: file.selected,
  })) ?? [];
}

function isGenericBatchTitle(title: string): boolean {
  return /^(?:网盘分享(?:文件|内容)|下载(?:文件|内容)|未命名(?:任务|文件)|任务名称)$/.test(title.trim());
}

function batchResourceTitle(resources: readonly ConfirmResource[]): string | undefined {
  const paths = resources.map(resource => resource.name.split(/[\\/]/).filter(Boolean)).filter(path => path.length > 0);
  if (paths.length !== resources.length || paths.length === 0) return undefined;
  if (paths.length === 1) return paths[0].at(-1);
  return batchResourceCommonDirectory(resources);
}

function batchResourceCommonDirectory(resources: readonly ConfirmResource[]): string | undefined {
  const paths = resources.map(resource => resource.name.split(/[\\/]/).filter(Boolean)).filter(path => path.length > 0);
  if (paths.length !== resources.length || paths.length === 0) return undefined;
  const commonDirectory = paths[0].slice(0, -1).filter((part, index) => paths.every(path => path[index] === part));
  return commonDirectory.length > 0 ? commonDirectory.join('/') : undefined;
}

type BatchResourceFilter = 'all' | 'video' | 'image' | 'other';
type BatchResourceSortMode = 'name-asc' | 'name-desc' | 'size-asc' | 'size-desc';
type BatchResourceTreeNode =
  | { kind: 'file'; key: string; name: string; resource: ConfirmResource; bytes: number }
  | { kind: 'directory'; key: string; name: string; resources: readonly ConfirmResource[]; children: readonly BatchResourceTreeNode[]; bytes: number };

function batchResourceCategory(resource: ConfirmResource): Exclude<BatchResourceFilter, 'all'> {
  if (/\.(?:3gp|avi|flv|m4v|mkv|mov|mp4|mpeg|mpg|rmvb|ts|webm|wmv)$/i.test(resource.name)) return 'video';
  if (/\.(?:avif|bmp|gif|heic|heif|jpe?g|png|svg|tiff?|webp)$/i.test(resource.name)) return 'image';
  return 'other';
}

function batchResourceBytes(value: string): number {
  const matches = Array.from(value.matchAll(/(\d+(?:\.\d+)?)\s*(B|KB|MB|GB|TB)\b/gi));
  const match = matches[matches.length - 1];
  if (!match) return 0;
  const units = { B: 1, KB: 1024, MB: 1024 ** 2, GB: 1024 ** 3, TB: 1024 ** 4 };
  return Number(match[1]) * units[match[2].toUpperCase() as keyof typeof units];
}

function BatchSelectionCheckbox({ ariaLabel, checked, indeterminate, disabled, onChange }: { ariaLabel: string; checked: boolean; indeterminate: boolean; disabled: boolean; onChange(): void }) {
  const input = useRef<HTMLInputElement>(null);
  useEffect(() => {
    if (input.current) input.current.indeterminate = indeterminate;
  }, [indeterminate]);
  return <input ref={input} type="checkbox" aria-label={ariaLabel} checked={checked} disabled={disabled} onChange={onChange} />;
}

function batchResourceRootPrefixLength(resources: readonly ConfirmResource[], rootName: string): number {
  const paths = resources.map(resource => resource.name.split(/[\\/]/).filter(Boolean));
  if (!paths.length) return 0;
  const rootPositions = paths.map(parts => parts.findIndex(part => part === rootName));
  if (rootPositions[0] >= 0 && rootPositions.every(position => position === rootPositions[0])) {
    return rootPositions[0] + 1;
  }
  const first = paths[0][0];
  return first && paths.every(parts => parts.length > 1 && parts[0] === first) ? 1 : 0;
}

function buildBatchResourceTree(resources: readonly ConfirmResource[], rootName: string, sortMode?: BatchResourceSortMode): readonly BatchResourceTreeNode[] {
  const prefixLength = batchResourceRootPrefixLength(resources, rootName);
  const entries = resources.map(resource => {
    const original = resource.name.split(/[\\/]/).filter(Boolean);
    const parts = original.slice(prefixLength);
    return { resource, parts: parts.length ? parts : [original.at(-1) ?? resource.name] };
  });
  const build = (current: typeof entries, parentKey: string): BatchResourceTreeNode[] => {
    const directories = new Map<string, typeof entries>();
    const files: BatchResourceTreeNode[] = [];
    current.forEach(entry => {
      if (entry.parts.length <= 1) {
        files.push({ kind: 'file', key: `file:${entry.resource.id}`, name: entry.parts[0], resource: entry.resource, bytes: batchResourceBytes(entry.resource.size) });
        return;
      }
      const [directory, ...rest] = entry.parts;
      const grouped = directories.get(directory) ?? [];
      grouped.push({ resource: entry.resource, parts: rest });
      directories.set(directory, grouped);
    });
    const nodes: BatchResourceTreeNode[] = [
      ...Array.from(directories, ([name, children]) => {
        const key = `${parentKey}/${name}`;
        const descendants = children.map(child => child.resource);
        return { kind: 'directory' as const, key, name, resources: descendants, children: build(children, key), bytes: descendants.reduce((sum, resource) => sum + batchResourceBytes(resource.size), 0) };
      }),
      ...files,
    ];
    if (!sortMode) return nodes;
    return nodes.sort((left, right) => {
      const comparison = sortMode.startsWith('name')
        ? left.name.localeCompare(right.name, 'zh-Hans-CN')
        : left.bytes - right.bytes;
      return sortMode.endsWith('asc') ? comparison : -comparison;
    });
  };
  return build(entries, '');
}

function BatchResourceTree({ nodes, disabled, expandedDirectories, depth = 0, onToggleDirectory, onToggleResource }: {
  nodes: readonly BatchResourceTreeNode[];
  disabled: boolean;
  expandedDirectories: ReadonlySet<string>;
  depth?: number;
  onToggleDirectory(key: string): void;
  onToggleResource(resourceId: string): void;
}) {
  return <>{nodes.map(node => {
    if (node.kind === 'file') {
      return <div key={node.key} className="batch-tree-file" style={{ paddingLeft: `${depth * 18}px` }}><ResourceFileRow resource={node.resource} displayName={node.name} disabled={disabled} showExtension compact onToggle={() => onToggleResource(node.resource.id)} /></div>;
    }
    const expanded = expandedDirectories.has(node.key);
    const selectedCount = node.resources.filter(resource => resource.selected).length;
    const allSelected = node.resources.length > 0 && selectedCount === node.resources.length;
    const toggleSelection = () => node.resources.forEach(resource => {
      if (resource.selected === allSelected) onToggleResource(resource.id);
    });
    return <div key={node.key} className="batch-directory-branch">
      <div className="batch-directory-row" style={{ paddingLeft: `${depth * 18 + 8}px` }}>
        <BatchSelectionCheckbox ariaLabel={t('batch.selectDirectory', { name: node.name })} checked={allSelected} indeterminate={selectedCount > 0 && !allSelected} disabled={disabled} onChange={toggleSelection} />
        <button type="button" className="batch-expand" aria-label={t(expanded ? 'batch.collapseDirectory' : 'batch.expandDirectory', { name: node.name })} aria-expanded={expanded} onClick={() => onToggleDirectory(node.key)}><svg className={expanded ? 'expanded' : ''} aria-hidden="true" viewBox="0 0 24 24"><path d="m9 5 7 7-7 7" /></svg></button>
        <span className="batch-directory-icon" aria-hidden="true">▰</span>
        <strong title={node.name}>{node.name}</strong>
        <small>{t('batch.itemsCount', { count: node.resources.length })}</small>
        <small>{formatBytes(node.bytes)}</small>
      </div>
      {expanded && <BatchResourceTree nodes={node.children} disabled={disabled} expandedDirectories={expandedDirectories} depth={depth + 1} onToggleDirectory={onToggleDirectory} onToggleResource={onToggleResource} />}
    </div>;
  })}</>;
}

type TranslationKey = Parameters<typeof t>[0];
const batchResourceSortGroups: ReadonlyArray<{ label: TranslationKey; options: ReadonlyArray<{ mode: BatchResourceSortMode; label: TranslationKey; ariaLabel: TranslationKey }> }> = [
  { label: 'download.sort.fileName', options: [
    { mode: 'name-asc', label: 'download.sort.aToZ', ariaLabel: 'download.sort.fileNameAToZ' },
    { mode: 'name-desc', label: 'download.sort.zToA', ariaLabel: 'download.sort.fileNameZToA' },
  ] },
  { label: 'download.sort.fileSize', options: [
    { mode: 'size-asc', label: 'download.sort.smallToLarge', ariaLabel: 'download.sort.fileSizeSmallToLarge' },
    { mode: 'size-desc', label: 'download.sort.largeToSmall', ariaLabel: 'download.sort.fileSizeLargeToSmall' },
  ] },
];

function BatchResourceSortControl({ value, onChange }: { value?: BatchResourceSortMode; onChange(mode: BatchResourceSortMode): void }) {
  const { t: translate } = useTranslation();
  const [open, setOpen] = useState(false);
  const rootRef = useRef<HTMLDivElement>(null);
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
  return <div className="task-sort-control batch-resource-sort-control" ref={rootRef}>
    <button type="button" className="task-sort-trigger" aria-label={translate('batch.sort.files')} aria-haspopup="menu" aria-expanded={open} onClick={() => setOpen(current => !current)}>
      <svg aria-hidden="true" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round"><path d="M10 6h9M10 12h6M10 18h3" /><path d="M5 5v14m-3-3 3 3 3-3" /></svg>
    </button>
    {open && <div className="task-sort-menu" role="menu" aria-label={translate('batch.sort.menu')}>
      {batchResourceSortGroups.map(group => <section key={group.label}><p>{translate(group.label)}</p><div>{group.options.map(option => <button type="button" key={option.mode} role="menuitemradio" aria-checked={value === option.mode} aria-label={translate(option.ariaLabel)} className={value === option.mode ? 'selected' : ''} onClick={() => { onChange(option.mode); setOpen(false); }}>{translate(option.label)}</button>)}</div></section>)}
    </div>}
  </div>;
}

function BatchResourceList({ resources, disabled, rootName, rootRow, onToggle }: { resources: readonly ConfirmResource[]; disabled: boolean; rootName: string; rootRow: ReactNode; onToggle(resourceId: string): void }) {
  const [filter, setFilter] = useState<BatchResourceFilter>('all');
  const [sortMode, setSortMode] = useState<BatchResourceSortMode>();
  const [expandedDirectories, setExpandedDirectories] = useState<Set<string>>(() => new Set());
  const filteredResources = filter === 'all' ? resources : resources.filter(resource => batchResourceCategory(resource) === filter);
  const visibleResources = filteredResources;
  const tree = buildBatchResourceTree(visibleResources, rootName, sortMode);
  const selectedCount = resources.filter(resource => resource.selected).length;
  const selectedBytes = resources.reduce((total, resource) => total + (resource.selected ? batchResourceBytes(resource.size) : 0), 0);
  const selectedVisibleCount = visibleResources.filter(resource => resource.selected).length;
  const allVisibleSelected = visibleResources.length > 0 && selectedVisibleCount === visibleResources.length;
  const toggleVisibleSelection = () => visibleResources.forEach(resource => {
    if (resource.selected === allVisibleSelected) onToggle(resource.id);
  });
  const toggleDirectory = (key: string) => setExpandedDirectories(current => {
    const next = new Set(current);
    if (next.has(key)) next.delete(key);
    else next.add(key);
    return next;
  });
  return <div className="batch-resource-card">
    <div className="parse-resource-toolbar batch-resource-toolbar">
  <label className="parse-select-all"><BatchSelectionCheckbox ariaLabel={t('batch.selectFiltered')} checked={allVisibleSelected} indeterminate={selectedVisibleCount > 0 && !allVisibleSelected} disabled={disabled} onChange={toggleVisibleSelection} /><span>{filter === 'all' ? t('batch.selectAll') : t('batch.selectCategory')}</span></label>
      <b>{t('batch.selectedSummary', { selected: selectedCount, total: resources.length, size: formatBytes(selectedBytes) })}</b>
      <div className="parse-resource-filters" role="group" aria-label={t('batch.filter.types')}>
        {(['all', 'video', 'image', 'other'] as BatchResourceFilter[]).map(option => <button key={option} type="button" className={filter === option ? 'active' : ''} onClick={() => setFilter(option)}>{t(`batch.filter.${option}` as TranslationKey)}</button>)}
      </div>
      <BatchResourceSortControl value={sortMode} onChange={setSortMode} />
    </div>
    {rootRow}
    <div className="resource-list batch-resource-list"><BatchResourceTree nodes={tree} disabled={disabled} expandedDirectories={expandedDirectories} onToggleDirectory={toggleDirectory} onToggleResource={onToggle} /></div>
  </div>;
}

function BatchLoadingStatus({ children }: { children: ReactNode }) {
  return <span className="batch-loading-status"><span>{children}</span><i className="task-metadata-indicator" aria-hidden="true" /></span>;
}

function BatchNewDownloadDialog({
  open,
  placement = 'modal',
  initialInput = '',
  onParse,
  onScan,
  onDiscardPrepared,
  onSubmitBatch,
  onClose,
  downloadDirectory,
  browseDirectory,
  privacyEnabled = false,
}: NewDownloadDialogProps) {
  const { t } = useTranslation();
  const batch = useDownloadDraftBatch({
    scan: onScan ?? (async input => [{ input, valid: true, kind: 'direct' }]),
    prepare: async (input, accessCode, probeVideo) => (await (probeVideo ? onParse(input, accessCode, true) : onParse(input, accessCode))) ?? { confirmation: { taskId: input, title: input, requiresConfirmation: true, resources: [], processing: [] } },
    discard: onDiscardPrepared ?? (async () => {}),
    concurrency: 4,
  });
  const [showPostProcessing, setShowPostProcessing] = useState(false);
  const [postProcessingNote, setPostProcessingNote] = useState('');
  const [manualInput, setManualInput] = useState('');
  const [scanning, setScanning] = useState(false);
  const [destination, setDestination] = useState<string | undefined>();
  const [submitting, setSubmitting] = useState(false);
  const [closing, setClosing] = useState(false);
  const [submitError, setSubmitError] = useState<string | null>(null);
  const previousFocusRef = useRef<HTMLElement | null>(null);
  const dialogRef = useRef<HTMLElement>(null);

  useLayoutEffect(() => {
    if (!open || placement !== 'modal') return;
    previousFocusRef.current = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    return () => {
      if (previousFocusRef.current?.isConnected) previousFocusRef.current.focus();
    };
  }, [open, placement]);

  useEffect(() => {
    if (!open) {
      void batch.discardAll();
      return;
    }
    setSubmitting(false);
    setClosing(false);
    setSubmitError(null);
    setShowPostProcessing(false);
    setPostProcessingNote('');
    setDestination(undefined);
    setManualInput(initialInput);
    setScanning(Boolean(initialInput.trim()));
    let current = true;
    void batch.start(initialInput).finally(() => { if (current) setScanning(false); });
    return () => { current = false; };
  }, [open, initialInput]);

  useEffect(() => {
    const pending = batch.items.filter(item => item.status === 'ready'
      && item.result?.confirmation?.isBitTorrent
      && (item.result.confirmation.fileListPending ?? item.result.confirmation.resources.length === 0));
    if (!pending.length || !isTauriRuntime()) return;
    let disposed = false;
    const refreshMetadata = () => {
      void listPersistedTasks().then((tasks) => {
        if (disposed) return;
        pending.forEach((item) => {
          const task = tasks.find(candidate => candidate.id === item.result?.confirmation?.taskId);
          const files = task?.files;
          if (!files?.length || (task?.metadata_gid && task.followed_by_gid !== task.aria_gid)) return;
          batch.updateResources(item.id, files.map(file => ({
            id: String(file.index), index: file.index, name: file.path,
            size: `${file.completed_length} B / ${file.length} B`, selected: file.selected,
          })), false);
        });
      }).catch(() => undefined);
    };
    refreshMetadata();
    const timer = window.setInterval(refreshMetadata, 500);
    return () => { disposed = true; window.clearInterval(timer); };
  }, [batch.items, batch.updateResources]);

  function scanManualInput(event: FormEvent) {
    event.preventDefault();
    if (!manualInput.trim() || scanning) return;
    setScanning(true);
    void batch.start(manualInput).finally(() => setScanning(false));
  }

  async function requestClose() {
    if (submitting || closing) return;
    setClosing(true);
    await batch.discardAll();
    onClose();
  }

  async function submit(privateDownload = false) {
    if (!canSubmit || submitting || !onSubmitBatch) return;
    const selected = batch.items.filter(item => item.selected);
    const ids = selected.map(item => item.id);
    batch.markSubmitted(ids);
    setSubmitting(true);
    setSubmitError(null);
    try {
      const failures = await onSubmitBatch(selected, destination, privateDownload, postProcessingNote);
      if (failures?.length) {
        batch.applySubmissionFailures(selected, failures);
        setSubmitting(false);
        return;
      }
      await batch.discardAll(ids[0]);
    } catch (reason) {
      batch.markSubmitted(ids, false);
      setSubmitError(reason instanceof Error ? reason.message : String(reason));
      setSubmitting(false);
    }
  }

  function trapFocus(event: React.KeyboardEvent<HTMLElement>) {
    if (event.key === 'Escape') {
      event.preventDefault();
      void requestClose();
      return;
    }
    if (placement !== 'modal' || event.key !== 'Tab') return;
    const focusable = Array.from(dialogRef.current?.querySelectorAll<HTMLElement>('button:not([disabled]), textarea:not([disabled]), input:not([disabled]), [tabindex]:not([tabindex="-1"])') ?? []);
    if (!focusable.length) return;
    const first = focusable[0];
    const last = focusable[focusable.length - 1];
    if (event.shiftKey && document.activeElement === first) { event.preventDefault(); last.focus(); }
    else if (!event.shiftKey && document.activeElement === last) { event.preventDefault(); first.focus(); }
  }

  if (!open) return null;
  const selectedCount = batch.items.filter(item => item.selected).reduce((count, item) =>
    count + (item.result?.confirmation?.video ? item.result.confirmation.resources.filter(r => r.selected).length : 1), 0);
  const hasUnresolvedBitTorrentFileList = batch.items.some(item => item.selected
    && item.status === 'ready'
    && item.result?.confirmation?.isBitTorrent
    && (item.result.confirmation.fileListPending ?? batchItemResources(item).length === 0)
    && batchItemResources(item).length === 0);
  const canSubmit = batch.canSubmit && !hasUnresolvedBitTorrentFileList;
  const dialog = <aside ref={dialogRef} className={`new-download-dialog batch-download-dialog ${placement}${batch.items.some(i => i.result?.confirmation?.video) ? ' has-video-options' : ''}`} role="dialog" aria-modal={placement === 'modal' || undefined} aria-label={t('download.new')} onKeyDown={trapFocus}>
    <div className="launcher-title"><span>{t('download.newTask')}</span>{batch.items.length > 0 && <small>{selectedCount ? t('download.selection.count', { count: selectedCount }) : t('download.selectTasks')}</small>}<button type="button" aria-label={placement === 'modal' ? t('download.closeNew') : t('download.collapseInput')} disabled={submitting || closing} onClick={() => void requestClose()}>×</button></div>
    {!batch.items.length && !scanning ? <form className="batch-manual-input" onSubmit={scanManualInput}><label htmlFor="batch-download-input">{t('download.content')}</label><textarea id="batch-download-input" aria-label={t('download.content')} value={manualInput} onChange={event => setManualInput(event.target.value)} placeholder={t('download.inputPlaceholder')} /><div className="new-download-actions"><button type="submit" className="primary" disabled={!manualInput.trim()}>{t('download.parseTask')}</button></div></form> : <section className="batch-download-list" aria-label={t('download.pendingTasks')}>
      {batch.items.map(item => {
        const title = batchItemTitle(item);
        const resources = batchItemResources(item);
        const video = item.result?.confirmation?.video;
        if (video && item.status === 'ready') return <VideoFormatCard key={item.id} video={video} selected={item.selected} formatIds={resources.filter(r => r.selected).map(r => r.id)}
          onSelectVideo={() => batch.toggleItem(item.id)} onSelectFormat={id => batch.toggleResource(item.id, id)} onRemove={() => batch.remove(item.id)} disabled={submitting || closing} submissionError={item.error} onReparse={() => batch.retry(item.id)} />;
        const existingTask = Boolean(item.result?.duplicate);
        const busy = submitting || closing;
        const fileListPending = Boolean(item.result?.confirmation?.isBitTorrent
          && (item.result.confirmation.fileListPending ?? resources.length === 0));
        const waitingForBitTorrentFileList = fileListPending && resources.length === 0;
        const loadingStatus = item.status === 'parsing'
          ? t('download.parsingTask')
          : !existingTask && waitingForBitTorrentFileList
            ? t('download.retrievingInfo')
            : undefined;
        const itemStatus = item.status === 'ready'
          ? existingTask ? t('download.existingFiles', { count: resources.length }) : loadingStatus ?? t('download.availableItems', { count: resources.length || 1 })
          : item.error;
        const canExpand = item.result ? canExpandDownloadDraft(item.result) : false;
        const resourceBytes = resources.reduce((total, resource) => total + batchResourceBytes(resource.size), 0);
        const partiallySelected = item.selected && resources.some(resource => !resource.selected);
        const rootRow = <div className="batch-download-row batch-tree-root">
            <BatchSelectionCheckbox ariaLabel={t('batch.selectNamed', { name: item.input })} checked={item.selected && !partiallySelected} indeterminate={partiallySelected} disabled={existingTask || busy} onChange={() => batch.toggleItem(item.id)} />
            {canExpand ? <button type="button" className="batch-expand" aria-label={t(item.expanded ? 'batch.collapseNamed' : 'batch.expandNamed', { name: title })} aria-expanded={item.expanded} onClick={() => batch.toggleExpanded(item.id)}><svg className={item.expanded ? 'expanded' : ''} aria-hidden="true" viewBox="0 0 24 24"><path d="m9 5 7 7-7 7" /></svg></button> : <span className="batch-expand-placeholder" />}
            <span className={`batch-root-icon${item.result?.confirmation?.isBitTorrent ? ' torrent' : ''}`} aria-hidden="true">{item.result?.confirmation?.isBitTorrent ? 'BT' : resources.length > 1 ? '▱' : '↓'}</span>
            <div className="batch-download-name"><strong title={title}>{title}</strong><small>{loadingStatus ? <BatchLoadingStatus>{loadingStatus}</BatchLoadingStatus> : itemStatus}</small></div>
            {resources.length > 0 && <small className="batch-root-size">{formatBytes(resourceBytes)}</small>}
            {item.status === 'failed' && <button type="button" className="batch-retry-button" disabled={busy} onClick={() => batch.retry(item.id)}>{t('common.retryAction')}</button>}
            <button type="button" aria-label={t('batch.removeNamed', { name: title })} disabled={busy} onClick={() => batch.remove(item.id)}>×</button>
          </div>;
        return <article key={item.id} className={`batch-download-item ${item.status}${item.expanded && canExpand ? ' expanded' : ''}`}>
          {item.expanded && canExpand ? <BatchResourceList resources={resources} disabled={existingTask || busy} rootName={title} rootRow={rootRow} onToggle={resourceId => batch.toggleResource(item.id, resourceId)} /> : rootRow}
          {item.canProbeVideo && item.status !== 'parsing' && <div className="batch-video-probe">
            <button type="button" disabled={busy} onClick={() => batch.retry(item.id, !item.probeVideo)}>
              {t(item.probeVideo ? 'video.useDirect' : 'video.probe')}
            </button>
          </div>}
          {item.status === 'ready' && item.error && <p className="parse-failure" role="alert">{item.error}</p>}
          {item.status === 'access_code_required' && <div className="batch-access-code">
            <span>{item.error}</span><input type="password" aria-label={t('batch.accessCodeFor', { name: title })} autoComplete="off" disabled={busy} value={item.accessCode} onChange={event => batch.setAccessCode(item.id, event.target.value)} /><button type="button" className="batch-retry-button" disabled={busy || !item.accessCode.trim()} onClick={() => batch.retry(item.id)}>{t('download.parseAgain')}</button>
          </div>}
        </article>;
      })}
      {!batch.items.length && scanning && <p className="batch-empty"><BatchLoadingStatus>{t('download.parsingTask')}</BatchLoadingStatus></p>}
    </section>}
    <button type="button" className="disclosure batch-processing-disclosure" aria-label={t('download.postProcessingAndPassword')} aria-expanded={showPostProcessing} onClick={() => setShowPostProcessing(value => !value)}>
      <span>{t('download.postProcessingAndPassword')}</span>
      <svg className={showPostProcessing ? 'expanded' : ''} aria-hidden="true" viewBox="0 0 24 24"><path d="m6 9 6 6 6-6" /></svg>
    </button>
    {showPostProcessing && <textarea className="batch-processing-input" aria-label={t('download.postProcessingAndPassword')} value={postProcessingNote} onChange={event => setPostProcessingNote(event.target.value)} placeholder={t('download.postProcessingPlaceholder')} />}
    {submitError && <p className="parse-failure" role="alert">{submitError}</p>}
    <footer className="batch-download-footer">
      <div className="batch-download-destination"><span>{t('download.to')} <strong>{destination || downloadDirectory || t('download.localDirectory')}</strong></span><button type="button" className="parse-destination" onClick={() => void browseDirectory?.(destination || downloadDirectory || '').then(value => { if (value) setDestination(value); })}>{t('settings.directory.change')}</button></div>
      <div><button type="button" disabled={submitting || closing} onClick={() => void requestClose()}>{t('dialog.cancel')}</button>{privacyEnabled && <button type="button" disabled={!canSubmit || submitting} onClick={() => void submit(true)}>{t('download.newPrivateTask')}</button>}<button type="button" className="primary" disabled={!canSubmit || submitting} onClick={() => void submit(false)}>{submitting ? t('download.submitting') : t('download.newTask')}</button></div>
    </footer>
  </aside>;
  if (placement !== 'modal') return dialog;
  return <div className="modal-backdrop new-download-backdrop" data-testid="new-download-backdrop" onMouseDown={event => { if (event.target === event.currentTarget) event.preventDefault(); }}>{dialog}</div>;
}

function LegacyNewDownloadDialog({ open, placement = 'launcher', onParse, onScan, onClose, onOpenTaskList, initialInput }: NewDownloadDialogProps) {
  const { t } = useTranslation();
  const [input, setInput] = useState('');
  const [accessCode, setAccessCode] = useState('');
  const [accessCodePrompt, setAccessCodePrompt] = useState<string | null>(null);
  const [isSubmitting, setIsSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [batch, setBatch] = useState<DownloadBatch | null>(null);
  const unavailableShare = isUnavailableShare(error);
  const dialogRef = useRef<HTMLElement>(null);
  const inputRef = useRef<HTMLTextAreaElement>(null);
  const accessCodeRef = useRef<HTMLInputElement>(null);
  const previousFocusRef = useRef<HTMLElement | null>(null);
  const onCloseRef = useRef(onClose);

  useEffect(() => { onCloseRef.current = onClose; }, [onClose]);

  useLayoutEffect(() => {
    if (!open) return;
    if (placement === 'modal') previousFocusRef.current = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    inputRef.current?.focus();
    return () => {
      if (placement === 'modal' && previousFocusRef.current?.isConnected) previousFocusRef.current.focus();
    };
  }, [open, placement]);

  useEffect(() => {
    if (!open) { setIsSubmitting(false); setError(null); setAccessCode(''); setAccessCodePrompt(null); setBatch(null); }
  }, [open]);

  useEffect(() => {
    if (open) { setInput(initialInput ?? ''); setError(null); setBatch(null); }
  }, [initialInput, open]);

  useEffect(() => {
    if (accessCodePrompt) accessCodeRef.current?.focus();
  }, [accessCodePrompt]);

  function requestClose() {
    if (!isSubmitting) {
      setAccessCode('');
      setAccessCodePrompt(null);
      setBatch(null);
      onCloseRef.current();
    }
  }

  function openTaskList() {
    if (isSubmitting) return;
    void onOpenTaskList?.();
    requestClose();
  }

  function finishBatch() {
    setInput('');
    setAccessCode('');
    setAccessCodePrompt(null);
    setBatch(null);
    onCloseRef.current();
  }

  function advanceBatch(currentBatch: DownloadBatch) {
    const nextIndex = currentBatch.index + 1;
    setIsSubmitting(false);
    setAccessCode('');
    setAccessCodePrompt(null);
    setError(null);
    if (nextIndex >= currentBatch.entries.length) {
      finishBatch();
      return;
    }
    setBatch({ ...currentBatch, index: nextIndex });
  }

  async function parseCandidate(candidate: string, currentBatch?: DownloadBatch, probeVideo = false) {
    try {
      const code = accessCode.trim() || undefined;
      const result = await (probeVideo ? onParse(candidate, code, true) : onParse(candidate, code));
      if (result?.shareAccessCodeRequired) {
        setAccessCodePrompt(result.shareAccessCodeRequired.message);
        setIsSubmitting(false);
        return;
      }
      if (currentBatch) {
        advanceBatch(currentBatch);
      } else {
        finishBatch();
      }
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : String(reason));
      setIsSubmitting(false);
    }
  }

  async function submitCurrentBatch(currentBatch: DownloadBatch) {
    const entry = currentBatch.entries[currentBatch.index];
    if (!entry.valid) {
      advanceBatch(currentBatch);
      return;
    }
    setIsSubmitting(true);
    setError(null);
    await parseCandidate(entry.input, currentBatch, entry.kind === 'web_video');
  }

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (isSubmitting) return;
    if (unavailableShare) { requestClose(); return; }
    if (batch) {
      await submitCurrentBatch(batch);
      return;
    }
    const text = input.trim();
    if (!text) return;
    setIsSubmitting(true);
    setError(null);
    try {
      const entries = onScan ? await onScan(text) : [];
      if (onScan && !(entries.length === 1 && directAutoStarts(entries[0], text))) {
    setBatch({ entries: entries.length ? entries : [{ input: text, valid: false, message: t('download.noSupportedLinks') }], index: 0 });
        setIsSubmitting(false);
        return;
      }
      await parseCandidate(text);
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : String(reason));
      setIsSubmitting(false);
    }
  }

  function trapModalFocus(event: React.KeyboardEvent<HTMLElement>) {
    if (event.key === 'Escape') {
      event.preventDefault();
      requestClose();
      return;
    }
    if (placement !== 'modal' || event.key !== 'Tab') return;
    const focusable = Array.from(dialogRef.current?.querySelectorAll<HTMLElement>('button:not([disabled]), textarea:not([disabled]), input:not([disabled]), select:not([disabled]), [tabindex]:not([tabindex="-1"])') ?? []).filter((element) => element.tabIndex >= 0);
    if (!focusable.length) return;
    const first = focusable[0];
    const last = focusable[focusable.length - 1];
    if (event.shiftKey && document.activeElement === first) {
      event.preventDefault();
      last.focus();
    } else if (!event.shiftKey && document.activeElement === last) {
      event.preventDefault();
      first.focus();
    }
  }

  if (!open) return null;

  const batchEntry = batch?.entries[batch.index];
  const batchIsLast = batch ? batch.index + 1 === batch.entries.length : false;
  const actionClass = accessCodePrompt ? 'primary cloud-toolbar-primary cloud-authorization-continuation' : 'primary';
  const parseAction = unavailableShare
    ? <button className={actionClass} type="button" onClick={requestClose}>{t('dialog.close')}</button>
    : batchEntry && !batchEntry.valid
      ? <button className={actionClass} type="button" disabled={isSubmitting} onClick={() => advanceBatch(batch!)}>{batchIsLast ? t('download.skipComplete') : t('download.skipContinue')}</button>
      : batchEntry
        ? <button className={actionClass} type="submit" disabled={isSubmitting}>{isSubmitting ? t('download.parsing') : batchIsLast ? t('download.confirmComplete') : t('download.confirmContinue')}</button>
    : <button className={actionClass} type="submit" disabled={isSubmitting || !input.trim()}>{isSubmitting ? t('download.parsing') : t('download.parseStart')}</button>;

  const dialog = (
    <aside ref={dialogRef} className={placement === 'launcher' ? 'launcher-panel new-download-dialog' : placement === 'header' ? 'new-download-dialog header-download-dialog' : 'new-download-dialog modal'} role="dialog" aria-modal={placement === 'modal' || undefined} aria-label={t('download.new')} onKeyDown={trapModalFocus}>
      <div className="launcher-title"><span>{t('download.new')}</span><button aria-label={placement === 'modal' ? t('download.closeNew') : t('download.collapseInput')} disabled={isSubmitting} onClick={requestClose}>×</button></div>
      <form onSubmit={submit} aria-busy={isSubmitting}>
        {placement !== 'launcher' && <label htmlFor="download-input">{t('download.content')}</label>}
        <textarea ref={inputRef} id="download-input" aria-label={t('download.content')} value={input} disabled={isSubmitting || !!batch} onChange={(event) => {
          const value = event.target.value;
          if (value.trim() !== input.trim()) { setError(null); setAccessCode(''); setAccessCodePrompt(null); }
          setInput(value);
        }} placeholder={t('download.inputPlaceholder')} />
        <div className={`new-download-actions${accessCodePrompt ? ' cloud-toolbar' : ''}`}>
          {placement === 'launcher' && <button type="button" onClick={openTaskList} disabled={isSubmitting}>{t('download.openTaskList')}</button>}
          {batch && <span role="status">{t('download.processingBatch', { current: batch.index + 1, total: batch.entries.length })}</span>}
          {batchEntry?.valid && <span>{t('download.validLinkConfirmation')}</span>}
          {batchEntry?.valid && batchEntry.canProbeVideo && <button type="button" disabled={isSubmitting} onClick={() => {
            setIsSubmitting(true);
            void parseCandidate(batchEntry.input, batch!, true);
          }}>{t('video.probe')}</button>}
          {batchEntry && !batchEntry.valid && <span role="alert" className="parse-failure">{batchEntry.message}</span>}
          {error && <span role="alert" className="parse-failure">{error}</span>}
          {accessCodePrompt ? <div className="new-download-tab-access-code">
            <span id="share-access-code-prompt" role="status">{accessCodePrompt}</span>
            <input ref={accessCodeRef} id="share-access-code" aria-label={t('download.accessCode')} aria-describedby="share-access-code-prompt" type="password" autoComplete="off" value={accessCode} disabled={isSubmitting} onChange={(event) => setAccessCode(event.target.value)} />
            {parseAction}
          </div> : parseAction}
        </div>
      </form>
    </aside>
  );

  if (placement === 'launcher' || placement === 'header') return dialog;
  return <div className="modal-backdrop new-download-backdrop" data-testid="new-download-backdrop" onMouseDown={(event) => { if (event.target === event.currentTarget) event.preventDefault(); }}>{dialog}</div>;
}
