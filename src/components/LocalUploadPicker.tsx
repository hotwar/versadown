import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { formatUploadTime, normalizeUploadSelection, uploadPathKey, uploadPathSegments, type LocalUploadEntry, type LocalUploadListing } from '../lib/localUpload';
import { formatBytes } from '../lib/downloadEvents';
import { t, useTranslation } from '../i18n';

interface LocalUploadPickerProps {
  listEntries(path?: string): Promise<LocalUploadListing>;
  onConfirm(paths: string[]): void;
  onCancel(): void;
}

type UploadSortColumn = 'name' | 'size' | 'modifiedAt';
type UploadSort = { column: UploadSortColumn; direction: 'ascending' | 'descending' };

const uploadNameCollator = new Intl.Collator(undefined, { numeric: true, sensitivity: 'base' });

function compareOptionalNumber(left: number | null | undefined, right: number | null | undefined): number {
  const leftMissing = left == null || !Number.isFinite(left);
  const rightMissing = right == null || !Number.isFinite(right);
  if (leftMissing !== rightMissing) return leftMissing ? 1 : -1;
  return leftMissing ? 0 : left! - right!;
}

function sortUploadEntries(entries: readonly LocalUploadEntry[], sort: UploadSort | null): LocalUploadEntry[] {
  if (!sort) return [...entries];
  return entries.map((entry, index) => ({ entry, index })).sort((left, right) => {
    if (left.entry.kind !== right.entry.kind) return left.entry.kind === 'directory' ? -1 : 1;
    let compared = sort.column === 'name'
      ? uploadNameCollator.compare(left.entry.name, right.entry.name)
      : compareOptionalNumber(left.entry[sort.column], right.entry[sort.column]);
    if (compared && sort.column !== 'name'
      && (left.entry[sort.column] == null || right.entry[sort.column] == null)) return compared;
    if (sort.direction === 'descending') compared *= -1;
    return compared || uploadNameCollator.compare(left.entry.name, right.entry.name) || left.index - right.index;
  }).map(({ entry }) => entry);
}

function EntryIcon({ directory }: { directory: boolean }) {
  return <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
    {directory ? <path d="M3 7V5a2 2 0 0 1 2-2h5l3 3h6a2 2 0 0 1 2 2v11a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V7Zm0 2h18" /> : <path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8l-6-6Zm0 0v6h6" />}
  </svg>;
}

export function LocalUploadPicker({ listEntries, onConfirm, onCancel }: LocalUploadPickerProps) {
  const { t: translate } = useTranslation();
  const [listing, setListing] = useState<LocalUploadListing | null>(null);
  const [pathInput, setPathInput] = useState('');
  const [editingPath, setEditingPath] = useState(false);
  const [selected, setSelected] = useState<LocalUploadEntry[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const [visibleCount, setVisibleCount] = useState(200);
  const [sort, setSort] = useState<UploadSort | null>(null);
  const [showHidden, setShowHidden] = useState(false);
  const request = useRef(0);
  const submitted = useRef(false);
  const dialogRef = useRef<HTMLDialogElement>(null);
  const pathRef = useRef<HTMLInputElement>(null);
  const breadcrumbRef = useRef<HTMLElement>(null);
  const pathClick = useRef<ReturnType<typeof setTimeout>>();
  const allRef = useRef<HTMLInputElement>(null);
  const selectionKeys = useMemo(() => new Set(selected.map(item => uploadPathKey(item.path))), [selected]);
  const normalized = useMemo(() => normalizeUploadSelection(selected), [selected]);
  const entries = useMemo(() => sortUploadEntries((listing?.entries ?? []).filter(entry => showHidden || !entry.name.startsWith('.')), sort), [listing?.entries, showHidden, sort]);
  const selectedHere = entries.filter(item => selectionKeys.has(uploadPathKey(item.path))).length;

  const load = useCallback(async (path?: string) => {
    clearTimeout(pathClick.current);
    pathClick.current = undefined;
    const id = ++request.current;
    setLoading(true); setError('');
    try {
      const next = await listEntries(path);
      if (id !== request.current) return;
      setListing(next); setPathInput(next.path); setVisibleCount(200); setEditingPath(false);
    } catch (failure) {
      if (id === request.current) setError(failure instanceof Error ? failure.message : typeof failure === 'string' ? failure : translate('upload.error.readDirectory'));
    } finally {
      if (id === request.current) setLoading(false);
    }
  }, [listEntries, translate]);

  const cancelPathClick = useCallback(() => { clearTimeout(pathClick.current); pathClick.current = undefined; }, []);
  useEffect(() => { void load(); return () => { request.current++; cancelPathClick(); }; }, [load, cancelPathClick]);
  useEffect(() => { cancelPathClick(); }, [listing?.path, cancelPathClick]);
  useEffect(() => {
    if (editingPath) { pathRef.current?.focus(); pathRef.current?.select(); }
  }, [editingPath]);
  useEffect(() => {
    const previous = document.activeElement;
    const dialog = dialogRef.current!;
    if (typeof dialog.showModal === 'function') dialog.showModal();
    else dialog.setAttribute('open', '');
    breadcrumbRef.current?.focus();
    return () => {
      if (typeof dialog.close === 'function') dialog.close();
      if (previous instanceof HTMLElement && previous.isConnected) previous.focus();
    };
  }, []);
  useEffect(() => { if (allRef.current) allRef.current.indeterminate = selectedHere > 0 && selectedHere < entries.length; }, [selectedHere, entries.length, loading]);

  function toggle(entry: LocalUploadEntry) {
    const key = uploadPathKey(entry.path);
    setSelected(current => current.some(item => uploadPathKey(item.path) === key)
      ? current.filter(item => uploadPathKey(item.path) !== key) : [...current, entry]);
  }

  function editPath() {
    cancelPathClick();
    setPathInput(listing?.path ?? '');
    setEditingPath(true);
  }

  function changeSort(column: UploadSortColumn) {
    setSort(current => current?.column === column
      ? { column, direction: current.direction === 'ascending' ? 'descending' : 'ascending' }
      : { column, direction: 'ascending' });
  }

  function sortHeader(column: UploadSortColumn, label: Parameters<typeof t>[0]) {
    const direction = sort?.column === column ? sort.direction : 'none';
    return <span role="columnheader" aria-sort={direction}>
      <button type="button" className="local-upload-sort-button" aria-label={translate('upload.sortBy', { label: translate(label) })} onClick={() => changeSort(column)}>
        {translate(label)}<span aria-hidden="true">{direction === 'none' ? '↕' : direction === 'ascending' ? '↑' : '↓'}</span>
      </button>
    </span>;
  }

  return createPortal(<dialog ref={dialogRef} className="local-upload-picker" aria-label={translate('upload.selectContent')} onCancel={event => { event.preventDefault(); onCancel(); }} onKeyDown={event => {
    event.stopPropagation();
    if (event.key === 'Escape') { event.preventDefault(); onCancel(); }
  }}>
    <header><h2>{translate('upload.selectContent')}</h2><button type="button" className="popup-close" aria-label={translate('upload.closeSelection')} onClick={onCancel}>×</button></header>
    <form className="local-upload-path" onSubmit={event => { event.preventDefault(); if (pathInput.trim()) void load(pathInput.trim()); }}>
      {editingPath ? <input ref={pathRef} aria-label={translate('upload.localPath')} value={pathInput} onChange={event => setPathInput(event.target.value)} placeholder={translate('upload.enterLocalPath')} onBlur={() => { if (!loading) setEditingPath(false); }} onKeyDown={event => {
        if (event.key === 'Escape') { event.preventDefault(); event.stopPropagation(); setEditingPath(false); requestAnimationFrame(() => breadcrumbRef.current?.focus()); }
      }} /> : <nav ref={breadcrumbRef} tabIndex={0} className="local-upload-breadcrumbs" aria-label={translate('upload.localPath')} onDoubleClick={editPath} onKeyDown={event => {
        if (event.key === 'F2' || (event.key === 'Enter' && event.target === event.currentTarget)) { event.preventDefault(); editPath(); }
      }}>
        {uploadPathSegments(listing?.path ?? '').map((segment, index) => <span key={segment.path}>{index > 0 && <i aria-hidden="true">›</i>}<button type="button" aria-label={translate('upload.goTo', { path: segment.path })} title={segment.path} onClick={event => {
          cancelPathClick();
          if (event.detail === 0) { void load(segment.path); return; }
          // Defer pointer navigation so a double click can edit the original path.
          if (event.detail === 1) pathClick.current = setTimeout(() => { void load(segment.path); }, (listing?.doubleClickTimeMs ?? 500) + 30);
        }}>{segment.name}</button></span>)}
      </nav>}
    </form>
    <div className="local-upload-body">
      <nav aria-label={translate('upload.localLocations')}><button type="button" onClick={() => { void load(); }}>{translate('upload.userFolder')}</button>{listing?.roots.map(root => <button type="button" key={root.path} title={root.path} onClick={() => { void load(root.path); }}>{root.name}</button>)}</nav>
      <section className="local-upload-files" aria-label={translate('upload.localFilesAndFolders')} aria-busy={loading}>
        {error && <p role="alert">{error}</p>}
        {loading ? <p role="status">{translate('upload.readingDirectory')}</p> : <>
          <div className="local-upload-select-all"><input ref={allRef} type="checkbox" aria-label={translate('upload.selectCurrentFolder')} disabled={!entries.length} checked={entries.length > 0 && selectedHere === entries.length} onChange={event => {
            const keys = new Set(entries.map(item => uploadPathKey(item.path)));
            setSelected(current => event.target.checked ? [...current, ...entries.filter(item => !selectionKeys.has(uploadPathKey(item.path)))] : current.filter(item => !keys.has(uploadPathKey(item.path))));
          }} />{sortHeader('name', 'download.sort.name')}{sortHeader('size', 'download.sort.size')}{sortHeader('modifiedAt', 'download.sort.time')}</div>
          <div className="local-upload-file-list">
            {listing?.parentPath && <div className="local-upload-entry local-upload-parent"><span /><button type="button" className="local-upload-entry-name" title={translate('upload.parentFolder')} onClick={() => { void load(listing.parentPath!); }}><EntryIcon directory /><span>..</span></button><span /><span /></div>}
            {entries.length === 0 && <p className="local-upload-empty">{translate('upload.folderEmpty')}</p>}
            {entries.slice(0, visibleCount).map(entry => <div className={`local-upload-entry${selectionKeys.has(uploadPathKey(entry.path)) ? ' selected' : ''}`} key={entry.path}>
              <input type="checkbox" aria-label={translate('upload.selectNamed', { name: entry.name })} checked={selectionKeys.has(uploadPathKey(entry.path))} onChange={() => toggle(entry)} />
              <button type="button" className="local-upload-entry-name" title={entry.path} onClick={() => toggle(entry)} onDoubleClick={() => { if (entry.kind === 'directory') void load(entry.path); }} onKeyDown={event => {
                if (event.key === 'Enter' && entry.kind === 'directory') { event.preventDefault(); void load(entry.path); }
              }}><EntryIcon directory={entry.kind === 'directory'} /><span>{entry.name}</span></button>
              <small>{entry.kind === 'directory' || entry.size == null ? '—' : formatBytes(entry.size)}</small>
              <small>{formatUploadTime(entry.modifiedAt)}</small>
            </div>)}
            {entries.length > visibleCount && <button type="button" onClick={() => setVisibleCount(count => count + 200)}>{translate('upload.showMore', { count: entries.length - visibleCount })}</button>}
          </div>
        </>}
      </section>
    </div>
    <footer><p>{translate('upload.itemsToUpload', { count: normalized.length })}</p><label className="local-upload-hidden-toggle"><input type="checkbox" checked={showHidden} onChange={event => setShowHidden(event.target.checked)} />{translate('upload.showHidden')}</label><button type="button" onClick={onCancel}>{translate('dialog.cancel')}</button><button type="button" className="primary" disabled={!normalized.length || loading} onClick={() => {
      if (submitted.current || !normalized.length) return;
      submitted.current = true;
      onConfirm(normalized.map(item => item.path));
    }}>{translate('upload.confirm')}</button></footer>
  </dialog>, document.body);
}
