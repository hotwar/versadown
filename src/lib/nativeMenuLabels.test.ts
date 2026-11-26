import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import en from '../i18n/messages/en';
import zhCN from '../i18n/messages/zh-CN';
import nativeMenuLabelsRust from '../../src-tauri/src/native_menu_labels.rs?raw';

const invoke = vi.hoisted(() => vi.fn().mockResolvedValue(undefined));
vi.mock('@tauri-apps/api/core', () => ({ invoke, Channel: class {} }));

async function freshModules() {
  vi.resetModules();
  const i18n = await import('../i18n');
  const labels = await import('./nativeMenuLabels');
  return { i18n, ...labels };
}

function pushedLabels() {
  const calls = invoke.mock.calls.filter(([command]) => command === 'set_native_menu_labels');
  return calls.map(([, args]) => (args as { labels: Record<string, string> }).labels);
}

describe('native menu labels', () => {
  beforeEach(() => {
    invoke.mockClear();
    window.localStorage.clear();
    vi.spyOn(navigator, 'languages', 'get').mockReturnValue(['zh-CN']);
    (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {};
  });
  afterEach(() => {
    delete (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__;
    vi.restoreAllMocks();
  });

  it('draws every label from a catalog entry that exists in zh-CN and en', async () => {
    const { nativeMenuLabelKeys } = await freshModules();
    for (const catalogKey of Object.values(nativeMenuLabelKeys)) {
      expect(zhCN, catalogKey).toHaveProperty(catalogKey);
      expect(en, catalogKey).toHaveProperty(catalogKey);
    }
  });

  it('uses the same keys as the native label table', async () => {
    const { nativeMenuLabelKeys } = await freshModules();
    const struct = nativeMenuLabelsRust.match(/pub struct NativeMenuLabels \{([^}]*)\}/)![1];
    const nativeKeys = [...struct.matchAll(/pub (\w+): String/g)]
      .map(([, field]) => field.replace(/_(\w)/g, (_, letter: string) => letter.toUpperCase()));
    expect(Object.keys(nativeMenuLabelKeys).sort()).toEqual(nativeKeys.sort());
  });

  it('pushes complete Simplified Chinese labels at startup', async () => {
    const { startNativeMenuLabelSync, nativeMenuLabelKeys } = await freshModules();
    startNativeMenuLabelSync();

    const [labels] = pushedLabels();
    expect(Object.keys(labels).sort()).toEqual(Object.keys(nativeMenuLabelKeys).sort());
    expect(Object.values(labels).every((label) => label.trim().length > 0)).toBe(true);
    expect(labels).toMatchObject({
      hideManager: '隐藏主界面',
      showManager: '显示主界面',
      newDownload: '新建下载',
      launcherVisibility: '悬浮球显示',
      quit: '退出万能下载',
    });
  });

  it('pushes the English labels after switching language, then back again', async () => {
    const { i18n, startNativeMenuLabelSync, nativeMenuLabelKeys } = await freshModules();
    startNativeMenuLabelSync();
    await i18n.setLocalePreference('en');

    const pushed = pushedLabels();
    expect(pushed).toHaveLength(2);
    expect(pushed[1]).toEqual(Object.fromEntries(
      Object.entries(nativeMenuLabelKeys).map(([key, catalogKey]) => [key, en[catalogKey]]),
    ));
    expect(pushed[1].hideManager).toBe('Hide main window');

    await i18n.setLocalePreference('zh-CN');
    expect(pushedLabels()[2].hideManager).toBe('隐藏主界面');
  });

  it('stops pushing once unsubscribed and never pushes outside the desktop app', async () => {
    const { i18n, startNativeMenuLabelSync } = await freshModules();
    const stop = startNativeMenuLabelSync();
    stop();
    await i18n.setLocalePreference('en');
    expect(pushedLabels()).toHaveLength(1);

    invoke.mockClear();
    delete (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__;
    startNativeMenuLabelSync();
    expect(invoke).not.toHaveBeenCalled();
  });
});
