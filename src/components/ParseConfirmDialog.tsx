import { useEffect, useRef, useState } from 'react';
import { ConfirmResource, ParseResult } from '../lib/types';
import { VideoFormatCard } from './VideoFormatCard';
import { ResourceFileRow, resourceFileCategory } from './ResourceFileRow';
import { t, useTranslation } from '../i18n';

interface ParseConfirmDialogProps {
  result: ParseResult;
  placement?: 'modal' | 'header';
  downloadDirectory?: string;
  browseDirectory?(currentDirectory: string): Promise<string | null>;
  privacyEnabled?: boolean;
  onConfirm(selectedIds: string[], destinationDirectory?: string, privateDownload?: boolean): void;
  onSelectionChange(selectedIds: string[]): void;
  onClose(): void;
}

type ResourceFilter = 'all' | 'video' | 'other';

function cloudShareProvider(planSource: string): string | undefined {
  if (!planSource.startsWith('cloud-share://')) return undefined;
  return planSource.slice('cloud-share://'.length).split('/', 1)[0];
}

function pathParts(value: string): string[] {
  return value.trim().replace(/\\/g, '/').replace(/\/{2,}/g, '/').split('/').filter(Boolean);
}

function relativePathParts(value: string, downloadDirectory?: string): string[] {
  const parts = pathParts(value);
  const directory = pathParts(downloadDirectory ?? '');
  const sharesDirectory = directory.length > 0 && directory.every((part, index) => parts[index]?.toLocaleLowerCase() === part.toLocaleLowerCase());
  const relative = sharesDirectory ? parts.slice(directory.length) : parts;
  return /^[a-z]:$/i.test(relative[0] ?? '') ? relative.slice(1) : relative;
}

function torrentRoot(resources: readonly ConfirmResource[], downloadDirectory?: string): string | undefined {
  const candidates = resources.map((resource) => relativePathParts(resource.name, downloadDirectory));
  if (!candidates.length || candidates.some((parts) => parts.length < 2)) return undefined;
  const root = candidates[0][0];
  return candidates.every((parts) => parts[0] === root) ? root : undefined;
}

function displayResourceName(resource: ConfirmResource, root: string | undefined, downloadDirectory?: string): string {
  const parts = relativePathParts(resource.name, downloadDirectory);
  if (!parts.length) return resource.name;
  return root && parts[0] === root ? parts.slice(1).join('/') || root : parts.join('/');
}

function resourceCategory(resource: ConfirmResource): Exclude<ResourceFilter, 'all'> {
  return resourceFileCategory(resource);
}

function SelectAllCheckbox({ checked, indeterminate, onChange }: { checked: boolean; indeterminate: boolean; onChange(): void }) {
  const input = useRef<HTMLInputElement>(null);
  useEffect(() => {
    if (input.current) input.current.indeterminate = indeterminate;
  }, [indeterminate]);
  return <input ref={input} type="checkbox" aria-label={t('batch.selectAllFiles')} checked={checked} onChange={onChange} />;
}

export function ParseConfirmDialog({ result, placement = 'modal', downloadDirectory, browseDirectory, privacyEnabled = false, onConfirm, onSelectionChange, onClose }: ParseConfirmDialogProps) {
  const { t } = useTranslation();
  const resources = result.resources;
  const [showPassword, setShowPassword] = useState(false);
  const [showProcessing, setShowProcessing] = useState(false);
  const processing = result.processing.filter(step => step.trim().length > 0);
  const hasPassword = Boolean(result.discoveredPassword?.trim());
  const hasProcessing = processing.length > 0 || hasPassword;
  const [filter, setFilter] = useState<ResourceFilter>('all');
  const [destinationOverride, setDestinationOverride] = useState<string>();
  useEffect(() => setDestinationOverride(undefined), [downloadDirectory]);
  const destination = destinationOverride || downloadDirectory || '';
  const selected = resources.filter((item) => item.selected).map((item) => item.id);
  const canConfirm = resources.length === 0 ? !result.isBitTorrent : selected.length > 0;
  const visibleResources = filter === 'all' ? resources : resources.filter((resource) => resourceCategory(resource) === filter);
  const allSelected = resources.length > 0 && selected.length === resources.length;
  const root = result.isBitTorrent ? torrentRoot(resources, downloadDirectory) : undefined;
  const providerKey = cloudShareProvider(result.taskId);
  useEffect(() => {
    setShowProcessing(false);
    setShowPassword(false);
  }, [result.taskId]);
  const taskTitle = root || (result.title && !result.title.startsWith('magnet:') ? result.title : (result.isBitTorrent ? t('download.kind.torrentTask') : t('download.content')));
  const changeSelection = (resourceId: string) => {
    onSelectionChange(resources.map((item) => item.id === resourceId ? { ...item, selected: !item.selected } : item).filter((item) => item.selected).map((item) => item.id));
  };
  const toggleAll = () => onSelectionChange(allSelected ? [] : resources.map((resource) => resource.id));
  const chooseDestination = async () => {
    const selectedDirectory = await browseDirectory?.(destination || downloadDirectory || '');
    if (selectedDirectory) setDestinationOverride(selectedDirectory === downloadDirectory ? undefined : selectedDirectory);
  };
  const dialog = <section className={`confirm-dialog${placement === 'header' ? ' header-confirm-dialog' : ''}`} role="dialog" aria-modal={placement === 'modal' || undefined} aria-label={t('download.confirmContent')}>
    <header><div className="confirm-dialog-title"><span aria-hidden="true">↓</span><h2>{t('download.newTask')}</h2></div><button aria-label={t('download.closeConfirm')} onClick={onClose}>×</button></header>
    {!providerKey && !result.video && <div className="parse-task-summary"><span className={`parse-task-icon${result.isBitTorrent ? ' torrent' : ''}`} aria-hidden="true">{result.isBitTorrent ? 'BT' : '↓'}</span><div><strong title={taskTitle}>{taskTitle}</strong><p>{resources.length ? t('batch.selectedFiles', { selected: selected.length, total: resources.length }) : result.isBitTorrent ? t('download.connectingMetadata') : t('download.singleFile')}</p></div></div>}
    {result.video ? <div className="video-confirm-scroll"><VideoFormatCard video={result.video} selected={true} formatIds={selected} onSelectFormat={changeSelection} /></div> : <div className="parse-resource-card">
      {resources.length > 0 && <div className="parse-resource-toolbar">
        <label className="parse-select-all"><SelectAllCheckbox checked={allSelected} indeterminate={selected.length > 0 && !allSelected} onChange={toggleAll} /><span>{t('batch.selectAll')}</span></label>
        <b>{t('batch.selectedFiles', { selected: selected.length, total: resources.length })}</b>
        <div className="parse-resource-filters" role="group" aria-label={t('batch.filter.types')}>
          {(['all', 'video', 'other'] as ResourceFilter[]).map((option) => <button key={option} type="button" className={filter === option ? 'active' : ''} onClick={() => setFilter(option)}>{t(`batch.filter.${option}` as Parameters<typeof t>[0])}</button>)}
        </div>
      </div>}
      <div className="resource-list">{resources.length ? visibleResources.map((resource) => <ResourceFileRow key={resource.id} resource={resource} displayName={displayResourceName(resource, root, downloadDirectory)} onToggle={() => changeSelection(resource.id)} />) : <p className="resource-loading">{t('download.retrievingFileInfo')}</p>}</div>
    </div>}
    {hasProcessing && <button className="disclosure" onClick={() => setShowProcessing(!showProcessing)}>{showProcessing ? t('common.collapse') : t('common.view')} {t('download.postProcessingAndPassword')} {showProcessing ? '⌃' : '⌄'}</button>}
    {hasProcessing && showProcessing && <div className="processing-summary"><ul>{processing.map((step) => <li key={step}>{step}</li>)}</ul>{hasPassword && <p>{t('download.archivePassword')}：<code>{showPassword ? result.discoveredPassword : '••••••••'}</code><button onClick={() => setShowPassword(!showPassword)}>{showPassword ? t('common.hide') : t('common.show')}</button></p>}</div>}
    <footer><button type="button" className="parse-destination" aria-label={t('settings.directory.choose')} onClick={() => void chooseDestination()}><span>{t('download.to')}</span><strong title={destination || t('download.localDirectory')}>{destination || t('download.localDirectory')}</strong><i aria-hidden="true">{t('settings.directory.change')}</i></button><div><button onClick={onClose}>{t('common.back')}</button>{privacyEnabled && <button className="secondary" disabled={!canConfirm} onClick={() => onConfirm(selected, destinationOverride, true)}>{t('download.privateDownload')} <span>{selected.length}</span></button>}<button className="primary" disabled={!canConfirm} onClick={() => onConfirm(selected, destinationOverride)}>{t('download.downloadNow')} <span>{selected.length}</span></button></div></footer>
  </section>;
  return placement === 'header' ? dialog : <div className="modal-backdrop" role="presentation">{dialog}</div>;
}
