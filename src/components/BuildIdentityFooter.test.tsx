import { render, screen, waitFor } from '@testing-library/react';
import { beforeEach, expect, it, vi } from 'vitest';
import { BuildIdentityFooter } from './BuildIdentityFooter';

const invoke = vi.hoisted(() => vi.fn());
vi.mock('@tauri-apps/api/core', () => ({ invoke, isTauri: () => true }));

const commit = 'b529c14b8c9411f0176aa748507871549a3a860d';
beforeEach(() => { invoke.mockReset(); });

it('shows a short commit with the full build identity available on hover', async () => {
  invoke.mockResolvedValue({ commit, branch: 'codex/video-page-downloads', dirty: false });
  render(<BuildIdentityFooter />);
  const label = await screen.findByText('b529c14');
  expect(label.title).toContain(commit);
  expect(label.title).toContain('codex/video-page-downloads');
  expect(label.title).not.toContain('未提交');
  expect(invoke).toHaveBeenCalledWith('get_build_info');
});

it('distinguishes a modified build from the exact commit', async () => {
  invoke.mockResolvedValue({ commit, branch: null, dirty: true });
  render(<BuildIdentityFooter />);
  expect(await screen.findByText('b529c14*')).toHaveAttribute('title', expect.stringContaining('编译时包含未提交修改'));
});

it('does not invent a commit when build metadata is unavailable', async () => {
  invoke.mockRejectedValue(new Error('No desktop backend'));
  render(<BuildIdentityFooter />);
  await waitFor(() => expect(invoke).toHaveBeenCalledOnce());
  expect(screen.getByText('提交未知')).toBeVisible();
});
