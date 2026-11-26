import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { cloudProviderLabel } from '../i18n/cloudProviderLabel';
import { shareFailureMessage } from '../i18n/shareFailureMessage';
import { t } from '../i18n';
import type { CloudProvider } from './cloudTypes';

/** What happened to one file request the user started in the share window. */
export interface BrowserShareCapture {
  provider: CloudProvider;
  /** The canonical share page the window showed, matching `BrowserShareOpened.page`. */
  sharePage: string;
  outcome: 'queued' | 'duplicate' | 'failed';
  name: string;
  /** A localization key or short text from the backend; never provider text. */
  message?: string | null;
}

function windowTitle(provider: CloudProvider): string {
  return t('cloud.browserShare.windowTitle', { provider: cloudProviderLabel(provider, t) });
}

/** Opens the provider's own share page in the embedded window that captures file downloads. */
export function openBrowserShare(provider: CloudProvider, url: string, destinationDirectory?: string, privateDownload = false): Promise<void> {
  return invoke<void>('cloud_open_browser_share', { url, destinationDirectory, privateDownload, title: windowTitle(provider), failureNotice: t('cloud.browserShare.notCaptured') }).catch(localizeBrowserShareFailure);
}

function localizeBrowserShareFailure(error: unknown): never {
  const message = typeof error === 'string' ? error : (error as { message?: unknown })?.message;
  const translated = typeof message === 'string' ? shareFailureMessage(message) : undefined;
  throw translated ? new Error(translated) : error;
}

/** Opens again the share page a captured task came from; task state stays backend-owned. */
export function reopenBrowserShare(taskId: string, source: string | undefined): Promise<void> {
  const provider = /^cloud-share:\/\/([a-z0-9]+)\//.exec(source ?? '')?.[1] as CloudProvider | undefined;
  if (!provider) return Promise.reject(new Error(t('cloud.browserShare.failed')));
  return invoke<void>('cloud_reopen_browser_share', { taskId, title: windowTitle(provider), failureNotice: t('cloud.browserShare.notCaptured') }).catch(localizeBrowserShareFailure);
}

/** The notice text for one capture result. */
export function browserShareCaptureMessage(capture: BrowserShareCapture): { kind: 'success' | 'error'; message: string } {
  if (capture.outcome === 'queued') return { kind: 'success', message: t('cloud.browserShare.queued', { name: capture.name }) };
  if (capture.outcome === 'duplicate') return { kind: 'success', message: t('cloud.browserShare.duplicate', { name: capture.name }) };
  const text = capture.message ?? '';
  return { kind: 'error', message: shareFailureMessage(text) ?? text ?? t('cloud.browserShare.failed') };
}

export function subscribeBrowserShareCapture(receive: (capture: BrowserShareCapture) => void): Promise<UnlistenFn> {
  return listen<BrowserShareCapture>('cloud-browser-share-capture', (event) => receive(event.payload));
}

/** The share window of `sharePage` closed, whether or not a file was taken over first. */
export function subscribeBrowserShareClosed(receive: (sharePage: string) => void): Promise<UnlistenFn> {
  return listen<{ sharePage: string }>('cloud-browser-share-closed', (event) => receive(event.payload.sharePage));
}

/**
 * What a share window told the new-task dialog about one share. `id` makes
 * repeated signals distinct so the dialog reacts to each once.
 */
export interface BrowserShareSignal {
  id: number;
  sharePage: string;
  /** `finished`: a file was taken over (queued or already listed). `closed`: the window went away. */
  kind: 'finished' | 'closed';
}

/** The signal a capture result gives its draft; a failed capture leaves the window and draft as they are. */
export function captureSignalKind(capture: BrowserShareCapture): BrowserShareSignal['kind'] | null {
  return capture.outcome === 'failed' ? null : 'finished';
}
