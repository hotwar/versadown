import { render, screen } from '@testing-library/react';
import { beforeEach, expect, it, vi } from 'vitest';
import { invoke } from '@tauri-apps/api/core';
import { StartupNotice } from './StartupNotice';

vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }));
vi.mock('../lib/api', () => ({ isTauriRuntime: () => true }));

beforeEach(() => vi.mocked(invoke).mockReset());

it('explains credential unavailability while keeping the app usable', async () => {
  vi.mocked(invoke).mockResolvedValue({ credentialsUnavailable: true });
  render(<StartupNotice />);
  expect(await screen.findByRole('alert')).toHaveTextContent('普通下载仍可使用');
  expect(screen.getByRole('alert')).toHaveTextContent('隐私任务保持保护');
});

it('does not show a warning after healthy startup', async () => {
  vi.mocked(invoke).mockResolvedValue({ credentialsUnavailable: false });
  const { findByRole } = render(<StartupNotice />);
  await expect(findByRole('alert', {}, { timeout: 100 })).rejects.toThrow();
});
