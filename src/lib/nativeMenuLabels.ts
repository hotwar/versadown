import { invoke } from '@tauri-apps/api/core';
import { subscribeLocale, t, type TranslationKey } from '../i18n';
import { isTauriRuntime } from './api';

/**
 * Text of the tray menu, which the native side draws. The i18n catalogs stay
 * the only source of translations: each stable key below names the catalog
 * entry that supplies it, and the whole table is pushed to the backend.
 */
export const nativeMenuLabelKeys = {
  tooltip: 'app.title',
  hideManager: 'launcher.hideMainWindow',
  showManager: 'launcher.showMainWindow',
  newDownload: 'launcher.newDownload',
  startAll: 'launcher.startAll',
  pauseAll: 'launcher.pauseAll',
  launcherVisibility: 'settings.launcher.visibility',
  launcherAlways: 'launcher.visibility.always',
  launcherDownloading: 'launcher.visibility.downloading',
  launcherHidden: 'launcher.visibility.hidden',
  quit: 'launcher.quit',
} as const satisfies Record<string, TranslationKey>;

export type NativeMenuLabelKey = keyof typeof nativeMenuLabelKeys;
export type NativeMenuLabels = Record<NativeMenuLabelKey, string>;

export function currentNativeMenuLabels(): NativeMenuLabels {
  const entries = (Object.keys(nativeMenuLabelKeys) as NativeMenuLabelKey[])
    .map((key) => [key, t(nativeMenuLabelKeys[key])] as const);
  return Object.fromEntries(entries) as NativeMenuLabels;
}

async function pushNativeMenuLabels(): Promise<void> {
  try {
    await invoke('set_native_menu_labels', { labels: currentNativeMenuLabels() });
  } catch (error) {
    // The tray keeps its previous text; the next language change retries.
    console.error('Unable to update the native menu text:', error);
  }
}

/** Pushes the labels now and after every language change; returns the unsubscribe. */
export function startNativeMenuLabelSync(): () => void {
  if (!isTauriRuntime()) return () => undefined;
  void pushNativeMenuLabels();
  return subscribeLocale(() => void pushNativeMenuLabels());
}
