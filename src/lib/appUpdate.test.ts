import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { version } from '../../src-tauri/tauri.conf.json';
import updateCapability from '../../src-tauri/capabilities/update.json';
import type { AppUpdate } from './types';

const native = vi.hoisted(() => ({ invoke: vi.fn(), openUrl: vi.fn() }));
vi.mock('@tauri-apps/api/core', () => ({ invoke: native.invoke, Channel: class {} }));
vi.mock('@tauri-apps/plugin-opener', () => ({ openUrl: native.openUrl }));

import { checkForAppUpdate, openExternalUrl } from './api';

const update: AppUpdate = {
  version: '0.3.1', notes: 'Update notes', downloadUrl: 'https://www.versadown.com/#gate'
};

beforeEach(() => {
  Object.defineProperty(window, '__TAURI_INTERNALS__', { configurable: true, value: {} });
  native.invoke.mockReset();
  native.openUrl.mockReset();
});

afterEach(() => {
  Reflect.deleteProperty(window, '__TAURI_INTERNALS__');
  window.localStorage.removeItem('versadown.app-update.v1');
});

it('checks again after a no-update result and ignores the former twelve-hour cache', async () => {
  window.localStorage.setItem('versadown.app-update.v1', JSON.stringify({ appVersion: version, checkedAt: Date.now(), update: null }));
  native.invoke.mockResolvedValueOnce(null).mockResolvedValueOnce(update);
  await expect(checkForAppUpdate()).resolves.toBeNull();
  await expect(checkForAppUpdate()).resolves.toEqual(update);
  expect(native.invoke).toHaveBeenCalledTimes(2);
  expect(native.invoke).toHaveBeenLastCalledWith('check_for_app_update');
});

it('shares concurrent checks, then permits another network check', async () => {
  let resolve!: (value: AppUpdate | null) => void;
  native.invoke.mockReturnValueOnce(new Promise<AppUpdate | null>(done => { resolve = done; }));
  const first = checkForAppUpdate();
  const second = checkForAppUpdate();
  expect(native.invoke).toHaveBeenCalledTimes(1);
  resolve(update);
  await expect(Promise.all([first, second])).resolves.toEqual([update, update]);
  native.invoke.mockResolvedValueOnce(null);
  await expect(checkForAppUpdate()).resolves.toBeNull();
  expect(native.invoke).toHaveBeenCalledTimes(2);
});

it('surfaces a failed check and permits retry', async () => {
  native.invoke.mockRejectedValueOnce(new Error('offline')).mockResolvedValueOnce(update);
  await expect(checkForAppUpdate()).rejects.toThrow('offline');
  await expect(checkForAppUpdate()).resolves.toEqual(update);
});

it('does not report up-to-date in a browser without the native checker', async () => {
  Reflect.deleteProperty(window, '__TAURI_INTERNALS__');
  await expect(checkForAppUpdate()).rejects.toThrow();
  expect(native.invoke).not.toHaveBeenCalled();
});

it('opens the official download page with the matching main-window permission', async () => {
  native.openUrl.mockResolvedValueOnce(undefined);
  await openExternalUrl(update.downloadUrl);
  expect(native.openUrl).toHaveBeenCalledWith(update.downloadUrl);
  expect(updateCapability.windows).toEqual(['main']);
  expect(updateCapability.permissions).toEqual([
    { identifier: 'opener:allow-open-url', allow: [{ url: update.downloadUrl }] }
  ]);
});

it.each([
  'http://www.versadown.com/#gate',
  'https://www.versadown.com.evil.test/#gate',
  'https://www.versadown.com:8443/#gate',
  'https://user@www.versadown.com/#gate',
  'file:///C:/Windows/notepad.exe'
])('rejects an unauthorized destination: %s', async url => {
  await expect(openExternalUrl(url)).rejects.toThrow();
  expect(native.openUrl).not.toHaveBeenCalled();
});

it('propagates browser launch failures for the About page to display', async () => {
  native.openUrl.mockRejectedValueOnce(new Error('browser unavailable'));
  await expect(openExternalUrl(update.downloadUrl)).rejects.toThrow('browser unavailable');
});
