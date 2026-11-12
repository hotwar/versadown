import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { vi } from 'vitest';
import { AgentAccessSettings } from './AgentAccessSettings';
import { defaultSettings } from '../lib/api';

it('requires an explicit grant with a directory before enabling local access', async () => {
  const user = userEvent.setup();
  const save = vi.fn();
  render(<AgentAccessSettings onSave={save} />);
  expect(screen.getByRole('checkbox')).not.toBeChecked();
  expect(screen.getByText(/全部非隐私任务/)).toBeVisible();
  await user.click(screen.getByRole('checkbox'));
  await user.click(screen.getByRole('button', { name: '保存 Agent 授权' }));
  expect(save).not.toHaveBeenCalled();
  expect(screen.getByRole('status')).toHaveTextContent('请至少选择一个下载目录');
});

it('saves normalized directory input through the dedicated permission operation', async () => {
  const user = userEvent.setup();
  const saved = { enabled: true, allowedDirectories: ['C:\\下载 目录', 'D:\\files'] };
  const save = vi.fn().mockResolvedValue({ ...defaultSettings, agentAccess: saved });
  render(<AgentAccessSettings onSave={save} />);
  await user.click(screen.getByRole('checkbox'));
  fireEvent.change(screen.getByRole('textbox'), { target: { value: ' C:\\下载 目录 \nD:\\files\nC:\\下载 目录\n' } });
  await user.click(screen.getByRole('button', { name: '保存 Agent 授权' }));
  await waitFor(() => expect(save).toHaveBeenCalledWith(saved));
  expect(screen.getByRole('textbox')).toHaveValue('C:\\下载 目录\nD:\\files');
  expect(screen.getByRole('status')).toHaveTextContent('Agent 授权已保存');
});

it('allows revocation without any directory and does not imply task cancellation', async () => {
  const user = userEvent.setup();
  const save = vi.fn().mockResolvedValue({ ...defaultSettings, agentAccess: { enabled: false, allowedDirectories: [] } });
  render(<AgentAccessSettings initial={{ enabled: true, allowedDirectories: ['C:\\gone'] }} onSave={save} />);
  await user.click(screen.getByRole('checkbox'));
  fireEvent.change(screen.getByRole('textbox'), { target: { value: '' } });
  await user.click(screen.getByRole('button', { name: '保存 Agent 授权' }));
  await waitFor(() => expect(save).toHaveBeenCalledWith({ enabled: false, allowedDirectories: [] }));
  expect(screen.getByRole('checkbox')).not.toBeChecked();
  expect(screen.getByText(/不取消下载/)).toBeVisible();
});

it('shows a failed save and allows retry without falsely reporting success', async () => {
  const user = userEvent.setup();
  const save = vi.fn().mockRejectedValue(new Error('无法应用本机访问设置'));
  render(<AgentAccessSettings initial={{ enabled: true, allowedDirectories: ['C:\\Downloads'] }} onSave={save} />);
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
  await waitFor(() => expect(screen.getByRole('textbox')).toHaveValue('C:\\Selected'));
  expect(screen.getByRole('checkbox')).not.toBeChecked();
  expect(save).not.toHaveBeenCalled();
});
