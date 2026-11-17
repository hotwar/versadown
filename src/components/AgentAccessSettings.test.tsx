import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { vi } from 'vitest';
import { AgentAccessSettings } from './AgentAccessSettings';
import { defaultSettings } from '../lib/api';

it('allows query access without granting download or upload directories', async () => {
  const user = userEvent.setup();
  const permissions = { enabled: true, allowedDirectories: [], uploadDirectories: [], cloudGrants: [] };
  const save = vi.fn().mockResolvedValue({ ...defaultSettings, agentAccess: permissions });
  render(<AgentAccessSettings onSave={save} />);
  expect(screen.getByRole('checkbox')).not.toBeChecked();
  expect(screen.getByText(/全部非隐私任务/)).toBeVisible();
  await user.click(screen.getByRole('checkbox'));
  await user.click(screen.getByRole('button', { name: '保存 Agent 授权' }));
  await waitFor(() => expect(save).toHaveBeenCalledWith(permissions));
  expect(screen.getByRole('status')).toHaveTextContent('Agent 授权已保存');
});

it('saves normalized directory input through the dedicated permission operation', async () => {
  const user = userEvent.setup();
  const saved = { enabled: true, allowedDirectories: ['C:\\下载 目录', 'D:\\files'], uploadDirectories: [], cloudGrants: [] };
  const save = vi.fn().mockResolvedValue({ ...defaultSettings, agentAccess: saved });
  render(<AgentAccessSettings onSave={save} />);
  await user.click(screen.getByRole('checkbox'));
  fireEvent.change(screen.getByRole('textbox', { name: '允许的下载目录' }), { target: { value: ' C:\\下载 目录 \nD:\\files\nC:\\下载 目录\n' } });
  await user.click(screen.getByRole('button', { name: '保存 Agent 授权' }));
  await waitFor(() => expect(save).toHaveBeenCalledWith(saved));
  expect(screen.getByRole('textbox', { name: '允许的下载目录' })).toHaveValue('C:\\下载 目录\nD:\\files');
  expect(screen.getByRole('status')).toHaveTextContent('Agent 授权已保存');
});

it('allows revocation without any directory and does not imply task cancellation', async () => {
  const user = userEvent.setup();
  const permissions = { enabled: false, allowedDirectories: [], uploadDirectories: [], cloudGrants: [] };
  const save = vi.fn().mockResolvedValue({ ...defaultSettings, agentAccess: permissions });
  render(<AgentAccessSettings initial={{ ...permissions, enabled: true, allowedDirectories: ['C:\\gone'] }} onSave={save} />);
  await user.click(screen.getByRole('checkbox'));
  fireEvent.change(screen.getByRole('textbox', { name: '允许的下载目录' }), { target: { value: '' } });
  await user.click(screen.getByRole('button', { name: '保存 Agent 授权' }));
  await waitFor(() => expect(save).toHaveBeenCalledWith(permissions));
  expect(screen.getByRole('checkbox')).not.toBeChecked();
  expect(screen.getByText(/不取消下载/)).toBeVisible();
});

it('shows a failed save and allows retry without falsely reporting success', async () => {
  const user = userEvent.setup();
  const save = vi.fn().mockRejectedValue(new Error('无法应用本机访问设置'));
  render(<AgentAccessSettings initial={{ enabled: true, allowedDirectories: ['C:\\Downloads'], uploadDirectories: [], cloudGrants: [] }} onSave={save} />);
  await user.click(screen.getByRole('button', { name: '保存 Agent 授权' }));
  await waitFor(() => expect(screen.getByRole('status')).toHaveTextContent('无法应用本机访问设置'));
  expect(screen.queryByText('Agent 授权已保存。')).not.toBeInTheDocument();
  expect(screen.getByRole('button', { name: '保存 Agent 授权' })).toBeEnabled();
});

it('adds an explicitly selected directory without enabling or saving automatically', async () => {
  const user = userEvent.setup();
  const browse = vi.fn().mockResolvedValue('C:\\Selected');
  const save = vi.fn();
  render(<AgentAccessSettings onSave={save} browseDirectory={browse} />);
  await user.click(screen.getByRole('button', { name: '添加目录' }));
  await waitFor(() => expect(screen.getByRole('textbox', { name: '允许的下载目录' })).toHaveValue('C:\\Selected'));
  expect(screen.getByRole('checkbox')).not.toBeChecked();
  expect(save).not.toHaveBeenCalled();
});

it('keeps selected upload source directories separate from download grants', async () => {
  const user = userEvent.setup();
  const browse = vi.fn().mockResolvedValue('C:\\Upload');
  const permissions = { enabled: true, allowedDirectories: [], uploadDirectories: ['C:\\Upload'], cloudGrants: [] };
  const save = vi.fn().mockResolvedValue({ ...defaultSettings, agentAccess: permissions });
  render(<AgentAccessSettings onSave={save} browseDirectory={browse} />);
  await user.click(screen.getByRole('checkbox'));
  await user.click(screen.getByRole('button', { name: '添加上传源目录' }));
  await waitFor(() => expect(screen.getByRole('textbox', { name: '允许读取并上传的本地目录' })).toHaveValue('C:\\Upload'));
  expect(screen.getByRole('textbox', { name: '允许的下载目录' })).toHaveValue('');
  await user.click(screen.getByRole('button', { name: '保存 Agent 授权' }));
  await waitFor(() => expect(save).toHaveBeenCalledWith(permissions));
});
