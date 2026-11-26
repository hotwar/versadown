import { render, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { vi } from 'vitest';
import { AgentAccessSettings } from './AgentAccessSettings';
import { defaultSettings } from '../lib/api';
vi.mock('./AgentConnect', () => ({ AgentConnect: () => <section aria-label="MCP 连接配置"><button type="button">复制 MCP JSON</button></section> }));
const empty = { enabled: false, allowedDirectories: [], uploadDirectories: [], cloudGrants: [] };

it('enables Agent access without prefilled local or cloud permissions', async () => {
  const user = userEvent.setup();
  const permissions = { ...empty, enabled: true };
  const save = vi.fn().mockResolvedValue({ ...defaultSettings, agentAccess: permissions });
  render(<AgentAccessSettings onSave={save} />);
  const area = within(screen.getByRole('region', { name: 'Agent 接入' }));
  const toggle = area.getByRole('checkbox', { name: '允许本机 Agent 调用' });
  const saveButton = area.getByRole('button', { name: '保存调用与授权' });
  const connection = area.getByRole('region', { name: 'MCP 连接配置' });
  expect(toggle.closest('form')).toContainElement(saveButton);
  expect(connection.closest('form')).toBeNull();
  expect(saveButton.compareDocumentPosition(connection)).toBe(Node.DOCUMENT_POSITION_FOLLOWING);
  expect(saveButton).toHaveAccessibleDescription('应用上方的调用开关和目录、网盘账号授权变更。');
  expect(screen.queryByRole('textbox')).not.toBeInTheDocument();
  expect(screen.queryByRole('combobox')).not.toBeInTheDocument();
  await user.click(toggle);
  await user.click(within(connection).getByRole('button', { name: '复制 MCP JSON' }));
  expect(save).not.toHaveBeenCalled();
  await user.click(saveButton);
  expect(save).toHaveBeenCalledWith(permissions);
  expect(screen.getByRole('status')).toHaveTextContent('调用与授权已保存');
});

it('shows saved local grants and persists selected removals', async () => {
  const user = userEvent.setup();
  const initial = { ...empty, enabled: true, allowedDirectories: ['C:/One', 'C:/Two'], uploadDirectories: ['C:/Upload'] };
  const expected = { ...initial, allowedDirectories: ['C:/Two'], uploadDirectories: [] };
  const save = vi.fn().mockResolvedValue({ ...defaultSettings, agentAccess: expected });
  render(<AgentAccessSettings initial={initial} onSave={save} />);
  expect(screen.getByText('C:/One')).toBeVisible();
  expect(screen.getByText('C:/Upload')).toBeVisible();
  await user.click(screen.getByRole('button', { name: '撤销第 1 项下载目录授权' }));
  await user.click(screen.getByRole('button', { name: '撤销第 1 项上传目录授权' }));
  expect(save).not.toHaveBeenCalled();
  await user.click(screen.getByRole('button', { name: '保存调用与授权' }));
  expect(save).toHaveBeenCalledWith(expected);
  expect(screen.getByText('C:/Two')).toBeVisible();
});

it('refreshes grants from another VD window and saves account revocation', async () => {
  const user = userEvent.setup();
  const initial = { ...empty, enabled: true };
  const authorized = { ...initial, cloudGrants: [{ provider: 'quark' as const, accountId: 'private-id', accountLabel: 'My account' }] };
  const save = vi.fn().mockResolvedValue({ ...defaultSettings, agentAccess: initial });
  const { rerender } = render(<AgentAccessSettings initial={initial} onSave={save} />);
  rerender(<AgentAccessSettings initial={authorized} onSave={save} />);
  expect(screen.getByText('夸克网盘 · My account')).toBeVisible();
  await user.click(screen.getByRole('button', { name: '撤销第 1 项网盘账号授权' }));
  await user.click(screen.getByRole('button', { name: '保存调用与授权' }));
  expect(save).toHaveBeenCalledWith(initial);
});

it('reports a failed save without claiming access changed', async () => {
  const user = userEvent.setup();
  const save = vi.fn().mockRejectedValue(new Error('无法应用本机访问设置'));
  render(<AgentAccessSettings initial={empty} onSave={save} />);
  await user.click(screen.getByRole('button', { name: '保存调用与授权' }));
  expect(screen.getByRole('status')).toHaveTextContent('无法应用本机访问设置');
  expect(screen.getByRole('button', { name: '保存调用与授权' })).toBeEnabled();
});
