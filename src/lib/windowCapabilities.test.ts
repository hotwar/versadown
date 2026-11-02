import { expect, it } from 'vitest';
import capabilities from '../../src-tauri/capabilities/default.json';

it('allows every native window command used to reveal the manager from the launcher', () => {
  expect(capabilities.permissions).toEqual(expect.arrayContaining([
    'core:window:allow-unminimize',
    'core:window:allow-show',
    'core:window:allow-set-focus'
  ]));
});

it('allows the public maximize toggle used by the window controls', () => {
  expect(capabilities.permissions).toContain('core:window:allow-toggle-maximize');
});
