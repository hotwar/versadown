import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { ConfirmResource, ParseDownloadResult, ScanDownloadProgress, ScanDownloadResult, VideoFormat } from '../lib/types';
import { t } from '../i18n';

export type DownloadDraftStatus = 'parsing' | 'ready' | 'access_code_required' | 'failed';

export interface DownloadDraftItem {
  id: string;
  input: string;
  kind?: string;
  canProbeVideo?: boolean;
  probeVideo?: boolean;
  status: DownloadDraftStatus;
  selected: boolean;
  result?: ParseDownloadResult;
  error?: string;
  accessCode: string;
  expanded: boolean;
  submitted: boolean;
  completedVideoSizes?: ReadonlySet<string>;
  generation: number;
}

interface Options {
  scan(input: string, onProgress?: (progress: ScanDownloadProgress) => void): Promise<ScanDownloadResult[]>;
  prepare(input: string, accessCode?: string, probeVideo?: boolean): Promise<ParseDownloadResult>;
  discard(reference: string): Promise<void>;
  concurrency?: number;
}

export interface DraftSubmissionFailure {
  id: string;
  message: string;
  resourceIds?: string[];
}

type WorkItem = { id: string; generation: number; accessCodeRetry?: boolean; superseded?: string };

function sizeKey(format: VideoFormat): string {
  return `${format.width}x${format.height}`;
}

// Extractor format IDs can change when a preview expires. Keep completed
// resolutions out of this draft even if the refreshed list uses different IDs.
function refreshVideoSelection(result: ParseDownloadResult, previous: DownloadDraftItem): ParseDownloadResult {
  const next = result.confirmation;
  const old = previous.result?.confirmation;
  if (!next?.video || (!old?.video && !previous.completedVideoSizes?.size)) return result;
  const formats = next.video.formats.filter(f => !previous.completedVideoSizes?.has(sizeKey(f)));
  if (!formats.length) throw new Error(t('video.error.formatGone'));
  const oldIds = new Set(old?.resources.filter(r => r.selected).map(r => r.id));
  const oldSizes = old?.video && new Set(old.video.formats.filter(f => oldIds.has(f.id)).map(sizeKey));
  const selected = new Set(formats.filter((f, index) => oldSizes ? oldSizes.has(sizeKey(f)) : index === 0).map(f => f.id));
  const available = new Set(formats.map(f => f.id));
  const resources = next.resources.filter(r => available.has(r.id)).map(r => ({ ...r, selected: selected.has(r.id) }));
  return { confirmation: { ...next, video: { ...next.video, formats }, resources } };
}

function errorMessage(reason: unknown): string {
  return reason instanceof Error ? reason.message : String(reason);
}

function isRejectedAccessCode(message: string): boolean {
  return /(?:访问码|分享码).*(?:错误|不正确|缺失)|提取码.*(?:错误|不正确|失败|缺失)/.test(message);
}

function resultReference(result?: ParseDownloadResult): string | undefined {
  return result?.confirmation?.taskId ?? result?.task?.id;
}

function hasRelativeDirectoryPath(path: string): boolean {
  if (/^(?:[a-z]:)?[\\/]/i.test(path)) return false;
  return path.split(/[\\/]/).filter(Boolean).length > 1;
}

export function hasExpandableResourceTree(resources: readonly Pick<ConfirmResource, 'name'>[]): boolean {
  return resources.length > 1 || resources.some(resource => hasRelativeDirectoryPath(resource.name));
}

export function canExpandDownloadDraft(result: ParseDownloadResult): boolean {
  if (result.confirmation?.video) return false;
  const task = result.task ?? result.duplicate?.task;
  if (task?.kind === 'folder') return true;
  const resources = result.confirmation?.resources
    ?? task?.files?.map(file => ({ name: file.path }))
    ?? [];
  return hasExpandableResourceTree(resources);
}

export function selectedResourceIds(item: DownloadDraftItem): string[] {
  return item.result?.confirmation?.resources
    .filter(resource => resource.selected)
    .map(resource => resource.id) ?? [];
}

function withResources(item: DownloadDraftItem, resources: ConfirmResource[], selected: boolean): DownloadDraftItem {
  const confirmation = item.result!.confirmation!;
  return { ...item, selected, result: { confirmation: { ...confirmation, resources } } };
}

// An item with files mirrors its files like a tri-state checkbox: picking any
// file selects the item, and toggling the item selects or clears every file.
function toggledItem(item: DownloadDraftItem): DownloadDraftItem {
  const resources = item.result?.confirmation?.resources ?? [];
  if (item.result?.confirmation?.video) {
    const choices = resources.some(r => r.selected) ? resources : resources.map((r, i) => ({ ...r, selected: i === 0 }));
    return withResources(item, choices, !item.selected);
  }
  if (!resources.length) return { ...item, selected: !item.selected };
  const selected = !(item.selected && resources.every(resource => resource.selected));
  return withResources(item, resources.map(resource => ({ ...resource, selected })), selected);
}

export function useDownloadDraftBatch({ scan, prepare, discard, concurrency = 4 }: Options) {
  const [items, setItems] = useState<DownloadDraftItem[]>([]);
  const itemsRef = useRef<DownloadDraftItem[]>([]);
  const queueRef = useRef<WorkItem[]>([]);
  const activeRef = useRef(0);
  const sessionRef = useRef(0);

  const replaceItems = useCallback((update: (current: DownloadDraftItem[]) => DownloadDraftItem[]) => {
    const next = update(itemsRef.current);
    itemsRef.current = next;
    setItems(next);
  }, []);

  const pumpRef = useRef<() => void>(() => {});
  pumpRef.current = () => {
    while (activeRef.current < Math.max(1, concurrency) && queueRef.current.length) {
      const work = queueRef.current.shift()!;
      const item = itemsRef.current.find(candidate => candidate.id === work.id && candidate.generation === work.generation);
      if (!item) continue;
      activeRef.current += 1;
      const prepareItem = async () => {
        if (work.superseded) {
          await discard(work.superseded);
          // A removed or closed draft must not start a second extraction.
          if (!itemsRef.current.some(candidate => candidate.id === work.id && candidate.generation === work.generation)) return;
          replaceItems(current => current.map(candidate => candidate.id === work.id ? { ...candidate, result: undefined } : candidate));
        }
        const accessCode = item.accessCode.trim() || undefined;
        return item.probeVideo ? prepare(item.input, accessCode, true) : prepare(item.input, accessCode);
      };
      void prepareItem()
        .then(result => {
          if (!result) return;
          const current = itemsRef.current.find(candidate => candidate.id === work.id && candidate.generation === work.generation);
          if (!current) {
            const reference = resultReference(result);
            if (reference) void discard(reference).catch(() => undefined);
            return;
          }
          let prepared: ParseDownloadResult;
          try {
            prepared = refreshVideoSelection(result, item);
          } catch (error) {
            const reference = resultReference(result);
            if (reference) void discard(reference).catch(() => undefined);
            throw error;
          }
          replaceItems(current => current.map(candidate => {
            if (candidate.id !== work.id || candidate.generation !== work.generation) return candidate;
            if (prepared.shareAccessCodeRequired) {
              return { ...candidate, result: prepared, status: 'access_code_required', error: prepared.shareAccessCodeRequired.message };
            }
            return {
              ...candidate,
              result: prepared,
              selected: candidate.selected && (!prepared.confirmation?.video || prepared.confirmation.resources.some(r => r.selected)),
              status: 'ready',
              error: undefined,
              expanded: candidate.expanded || canExpandDownloadDraft(prepared),
            };
          }));
        })
        .catch(reason => {
          const message = errorMessage(reason);
          replaceItems(current => current.map(candidate => {
            if (candidate.id !== work.id || candidate.generation !== work.generation) return candidate;
            if (work.accessCodeRetry && isRejectedAccessCode(message)) {
              return { ...candidate, status: 'access_code_required', accessCode: '', error: message };
            }
            return { ...candidate, status: 'failed', selected: candidate.probeVideo ? false : candidate.selected, error: message };
          }));
        })
        .finally(() => {
          activeRef.current -= 1;
          pumpRef.current();
        });
    }
  };

  // Detach the drafts synchronously so late scans and parse results cannot
  // re-enter a closed/replaced dialog. In-flight completions release their own references.
  const discardAll = useCallback(async (sessionItem?: string) => {
    if (sessionItem && !sessionItem.startsWith(`${sessionRef.current}-`)) return;
    ++sessionRef.current;
    const abandoned = itemsRef.current.filter(item => !item.submitted);
    itemsRef.current = [];
    queueRef.current = [];
    const references = abandoned.map(item => resultReference(item.result)).filter((ref): ref is string => !!ref);
    await Promise.allSettled([...new Set(references)].map(reference => discard(reference)));
  }, [discard]);

  const discardAllRef = useRef(discardAll);
  discardAllRef.current = discardAll;
  useEffect(() => () => { void discardAllRef.current(); }, []);

  const start = useCallback(async (input: string) => {
    void discardAll();
    const session = sessionRef.current;
    setItems([]);
    if (!input.trim()) return;
    const ids = new Map<number, string>();
    const released = new Set<string>();
    let finished = false;
    const receive = ({ entries, pending }: ScanDownloadProgress) => {
      if (finished || session !== sessionRef.current) return;
      const waiting = new Set(pending);
      const next = [...itemsRef.current];
      const work: WorkItem[] = [];
      for (const [position, entry] of entries.entries()) {
        let id = ids.get(position);
        let index = id ? next.findIndex(item => item.id === id) : -1;
        // Scan snapshots retain entry order, including repeated URLs. Keep
        // each row independent and never restore a removed or queued row.
        if (id && (index < 0 || released.has(id))) continue;
        if (!id) {
          id = `${session}-${position}`;
          ids.set(position, id);
          index = next.length;
          next.push({ id, input: entry.input, status: 'parsing', selected: entry.valid,
            accessCode: '', expanded: false, submitted: false, generation: 0 });
        }
        const item = next[index];
        next[index] = {
          ...item,
          kind: entry.valid ? entry.kind : undefined,
          canProbeVideo: entry.valid && entry.canProbeVideo,
          probeVideo: entry.valid && entry.kind === 'web_video',
          status: entry.valid ? 'parsing' : 'failed',
          selected: item.selected && entry.valid,
          error: entry.valid ? undefined : entry.message,
        };
        if (!waiting.has(entry.input)) {
          released.add(id);
          if (entry.valid) work.push({ id, generation: item.generation });
        }
      }
      itemsRef.current = next;
      setItems(next);
      queueRef.current.push(...work);
      pumpRef.current();
    };
    try {
      const entries = await scan(input, receive);
      receive({ entries: entries.length ? entries : [{ input, valid: false, message: t('download.noSupportedLinks') }], pending: [] });
    } catch (error) {
      if (session !== sessionRef.current) return;
      const message = errorMessage(error);
      if (!ids.size) receive({ entries: [{ input, valid: false, message }], pending: [] });
      else replaceItems(current => current.map(item => released.has(item.id) ? item : { ...item, status: 'failed', error: message }));
    } finally {
      finished = true;
    }
  }, [scan, discardAll, replaceItems]);

  const retry = useCallback((id: string, probeVideo?: boolean) => {
    let work: WorkItem | undefined;
    replaceItems(current => current.map(item => {
      if (item.id !== id || item.status === 'parsing' || (probeVideo !== undefined && !item.canProbeVideo)) return item;
      const generation = item.generation + 1;
      const mode = probeVideo ?? item.probeVideo;
      const superseded = item.result?.confirmation?.taskId;
      work = { id, generation, superseded, accessCodeRetry: item.status === 'access_code_required' };
      const selected = item.selected || probeVideo !== undefined || (item.status === 'failed' && mode === true);
      return { ...item, status: 'parsing', selected, probeVideo: mode, error: undefined, result: superseded ? item.result : undefined, generation };
    }));
    if (work) {
      queueRef.current.push(work);
      pumpRef.current();
    }
  }, [replaceItems]);

  const remove = useCallback((id: string) => {
    const item = itemsRef.current.find(candidate => candidate.id === id);
    const reference = item && !item.submitted ? resultReference(item.result) : undefined;
    replaceItems(current => current.filter(candidate => candidate.id !== id));
    if (reference) void discard(reference).catch(() => undefined);
  }, [discard, replaceItems]);

  const toggleItem = useCallback((id: string) => {
    replaceItems(current => current.map(item => item.id === id ? toggledItem(item) : item));
  }, [replaceItems]);

  const setAccessCode = useCallback((id: string, accessCode: string) => {
    replaceItems(current => current.map(item => item.id === id ? { ...item, accessCode } : item));
  }, [replaceItems]);

  const toggleExpanded = useCallback((id: string) => {
    replaceItems(current => current.map(item => item.id === id ? { ...item, expanded: !item.expanded } : item));
  }, [replaceItems]);

  const toggleResource = useCallback((id: string, resourceId: string) => {
    replaceItems(current => current.map(item => {
      if (item.id !== id || !item.result?.confirmation) return item;
      if (item.result.confirmation.video && !item.selected) {
        return withResources(item, item.result.confirmation.resources.map(r => ({ ...r, selected: r.id === resourceId })), true);
      }
      const resources = item.result.confirmation.resources.map(resource => resource.id === resourceId
        ? { ...resource, selected: !resource.selected }
        : resource);
      return withResources(item, resources, resources.some(resource => resource.selected));
    }));
  }, [replaceItems]);

  const updateResources = useCallback((id: string, resources: readonly ConfirmResource[], fileListPending = false) => {
    replaceItems(current => current.map(item => {
      if (item.id !== id || !item.result?.confirmation) return item;
      return {
        ...item,
        expanded: item.expanded || hasExpandableResourceTree(resources),
        result: {
          confirmation: {
            ...item.result.confirmation,
            fileListPending,
            resources: resources.map(resource => ({
              ...resource,
              selected: item.result?.confirmation?.resources.find(existing => existing.index === resource.index || existing.id === resource.id)?.selected ?? resource.selected,
            })),
          },
        },
      };
    }));
  }, [replaceItems]);

  const markSubmitted = useCallback((ids: string[], submitted = true) => {
    const targets = new Set(ids);
    replaceItems(current => current.map(item => targets.has(item.id) ? { ...item, submitted } : item));
  }, [replaceItems]);

  const applySubmissionFailures = useCallback((selected: DownloadDraftItem[], failures: DraftSubmissionFailure[]) => {
    const attempted = new Map(selected.map(item => [item.id, selectedResourceIds(item)]));
    const rejected = new Map(failures.map(failure => [failure.id, failure]));
    replaceItems(current => current.flatMap(item => {
      if (!attempted.has(item.id)) return [item];
      const failure = rejected.get(item.id);
      if (!failure) return [];
      const confirmation = item.result?.confirmation;
      if (!confirmation || !failure.resourceIds) return [{ ...item, submitted: false, error: failure.message }];
      const failedIds = new Set(failure.resourceIds);
      const completed = new Set(attempted.get(item.id)?.filter(id => !failedIds.has(id)));
      const resources = confirmation.resources.filter(r => !completed.has(r.id));
      const video = confirmation.video && { ...confirmation.video, formats: confirmation.video.formats.filter(f => !completed.has(f.id)) };
      const completedVideoSizes = new Set(item.completedVideoSizes);
      for (const format of confirmation.video?.formats ?? []) {
        if (completed.has(format.id)) completedVideoSizes.add(sizeKey(format));
      }
      return [{ ...item, completedVideoSizes, submitted: false, error: failure.message, result: { confirmation: { ...confirmation, video, resources } } }];
    }));
  }, [replaceItems]);

  const canSubmit = useMemo(() => {
    const selected = items.filter(item => item.selected);
    return selected.length > 0 && selected.every(item => item.status === 'ready'
      && (!item.result?.confirmation?.resources.length || item.result.confirmation.resources.some(resource => resource.selected)));
  }, [items]);

  return { items, canSubmit, start, retry, remove, toggleItem, setAccessCode, toggleExpanded, toggleResource, updateResources, markSubmitted, applySubmissionFailures, discardAll };
}
