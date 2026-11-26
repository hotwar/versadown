import { act, fireEvent, render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, vi } from 'vitest';
import AgentInteractionWindow, { type InteractionView } from './AgentInteractionWindow';

const mocked = vi.hoisted(() => ({ invoke: vi.fn(), close: vi.fn(), onChanged: undefined as undefined | (() => void) }));
vi.mock('@tauri-apps/api/event', () => ({ listen: vi.fn((_name: string, callback: () => void) => { mocked.onChanged = callback; return Promise.resolve(() => { mocked.onChanged = undefined; }); }) }));
vi.mock('@tauri-apps/api/core', () => ({ invoke: mocked.invoke }));
vi.mock('@tauri-apps/api/window', () => ({ getCurrentWindow: () => ({ close: mocked.close }) }));

const initial: InteractionView = {
  id: 'interaction-1', status: 'awaiting_user', kind: 'local_access',
  command: 'download', provider: null, accountLabel: null,
  downloadDirectories: [], uploadFiles: [], needsDirectory: true,
  expiresAt: 1800000000, errorCode: null,
};
let current: InteractionView | null;

beforeEach(() => {
  current = { ...initial };
  mocked.onChanged = undefined;
  mocked.close.mockReset().mockResolvedValue(undefined);
  mocked.invoke.mockReset().mockImplementation(async (command: string) => {
    if (command === 'agent_interaction_view') return current;
    if (command === 'agent_interaction_choose_directory') {
      current = { ...initial, needsDirectory: false, downloadDirectories: ['C:/下载 目录'] };
      return current;
    }
    if (command === 'agent_interaction_decide') {
      current = { ...current!, status: 'resuming' };
      return current;
    }
    throw new Error('unexpected command');
  });
});
afterEach(() => { vi.useRealTimers(); });

it('requires a native download-folder choice before approving this request', async () => {
  const user = userEvent.setup();
  render(<AgentInteractionWindow />);
  expect(screen.queryByRole('button', { name: '允许' })).not.toBeInTheDocument();
  const allow = await screen.findByRole('button', { name: '允许' });
  expect(allow).toBeDisabled();
  expect(screen.getByText(/本次允许仅适用于当前任务/)).toBeVisible();
  await user.click(screen.getByRole('button', { name: '选择下载目录' }));
  expect(mocked.invoke).toHaveBeenCalledWith('agent_interaction_choose_directory', { interactionId: initial.id });
  expect(await screen.findByText('C:/下载 目录')).toBeVisible();
  expect(allow).toBeEnabled();
  await user.click(allow);
  expect(mocked.invoke).toHaveBeenCalledWith('agent_interaction_decide', { interactionId: initial.id, allow: true });
  expect(await screen.findByText('正在继续任务…')).toBeVisible();
  expect(screen.queryByRole('button', { name: '允许' })).not.toBeInTheDocument();
});

it('shows account-wide permission and treats account names as plain text', async () => {
  const user = userEvent.setup();
  current = { ...initial, kind: 'cloud_account', provider: 'quark', accountLabel: '<img src=x onerror=alert(1)>', uploadFiles: ['C:/Upload/source.txt'], needsDirectory: false };
  render(<AgentInteractionWindow />);
  expect(await screen.findByText(/无需逐个目录授权/)).toBeVisible();
  expect(screen.getByText('<img src=x onerror=alert(1)>')).toBeVisible();
  expect(screen.queryByRole('img')).not.toBeInTheDocument();
  expect(screen.getByText(/本次允许仅适用于当前任务/)).toBeVisible();
  expect(screen.getByText('C:/Upload/source.txt')).toBeVisible();
  expect(screen.queryByRole('button', { name: '选择下载目录' })).not.toBeInTheDocument();
  await user.click(screen.getByRole('button', { name: '拒绝' }));
  expect(mocked.invoke).toHaveBeenCalledWith('agent_interaction_decide', { interactionId: initial.id, allow: false });
});

it('shows upload sources and cannot send a duplicate decision while resuming', async () => {
  current = { ...initial, uploadFiles: ['C:/private/source.txt'], needsDirectory: false };
  let resolveDecision!: () => void;
  const decision = new Promise<void>(resolve => { resolveDecision = resolve; });
  mocked.invoke.mockImplementation(async (command: string) => {
    if (command === 'agent_interaction_view') return current;
    if (command === 'agent_interaction_decide') { await decision; current = { ...current!, status: 'resuming' }; return current; }
    throw new Error('unexpected command');
  });
  render(<AgentInteractionWindow />);
  expect(await screen.findByText('C:/private/source.txt')).toBeVisible();
  const allow = screen.getByRole('button', { name: '允许' });
  fireEvent.click(allow);
  fireEvent.click(allow);
  expect(allow).toBeDisabled();
  expect(mocked.invoke.mock.calls.filter(([name]) => name === 'agent_interaction_decide')).toHaveLength(1);
  await act(async () => { resolveDecision(); });
  expect(screen.getByText('正在继续任务…')).toBeVisible();
});

it('polls login progress and stops polling once the request expires', async () => {
  vi.useFakeTimers();
  current = { ...initial, kind: 'cloud_login', status: 'awaiting_login', needsDirectory: false };
  await act(async () => { render(<AgentInteractionWindow />); });
  expect(screen.getByText('等待网盘登录完成')).toBeVisible();
  expect(screen.getByText(/不要把密码/)).toBeVisible();
  expect(screen.queryByRole('button', { name: '继续登录' })).not.toBeInTheDocument();
  current = { ...current, status: 'expired' };
  await act(async () => { vi.advanceTimersByTime(1000); });
  expect(screen.getByText(/授权请求已过期/)).toBeVisible();
  const calls = mocked.invoke.mock.calls.length;
  await act(async () => { vi.advanceTimersByTime(3000); });
  expect(mocked.invoke).toHaveBeenCalledTimes(calls);
  fireEvent.click(screen.getByRole('button', { name: '关闭' }));
  expect(mocked.close).toHaveBeenCalledTimes(1);
});

it('does not offer approval after a failed load and recovers with retry', async () => {
  const user = userEvent.setup();
  mocked.invoke.mockRejectedValueOnce(new Error('sensitive backend diagnostic'));
  render(<AgentInteractionWindow />);
  expect(await screen.findByRole('alert')).toHaveTextContent('无法读取授权请求');
  expect(screen.queryByText('sensitive backend diagnostic')).not.toBeInTheDocument();
  expect(screen.queryByRole('button', { name: '允许' })).not.toBeInTheDocument();
  await user.click(screen.getByRole('button', { name: '重试' }));
  expect(await screen.findByRole('button', { name: '允许' })).toBeDisabled();
});

it('reports a failed directory choice without allowing an incomplete request', async () => {
  const user = userEvent.setup();
  mocked.invoke.mockImplementation(async (command: string) => {
    if (command === 'agent_interaction_view') return current;
    throw new Error('chooser failed');
  });
  render(<AgentInteractionWindow />);
  await user.click(await screen.findByRole('button', { name: '选择下载目录' }));
  expect(await screen.findByRole('alert')).toHaveTextContent('操作未完成');
  expect(screen.getByRole('button', { name: '允许' })).toBeDisabled();
  expect(screen.getByRole('button', { name: '选择下载目录' })).toBeEnabled();
});

it('shows the final observed error code without an approval action', async () => {
  current = { ...initial, status: 'failed', errorCode: 'AUTH_REQUIRED' };
  render(<AgentInteractionWindow />);
  expect(await screen.findByText('AUTH_REQUIRED')).toBeVisible();
  expect(screen.getByText('未能完成')).toBeVisible();
  expect(screen.queryByRole('button', { name: '允许' })).not.toBeInTheDocument();
  expect(screen.getByRole('button', { name: '关闭' })).toBeEnabled();
});

it('reloads a reused terminal window when the backend explicitly resumes its request', async () => {
  vi.useFakeTimers();
  current = { ...initial, status: 'cancelled' };
  await act(async () => { render(<AgentInteractionWindow />); });
  expect(screen.getByText('已拒绝或取消')).toBeVisible();
  current = { ...initial, needsDirectory: false };
  await act(async () => { mocked.onChanged?.(); });
  expect(screen.getByRole('button', { name: '允许' })).toBeEnabled();
  const calls = mocked.invoke.mock.calls.length;
  await act(async () => { vi.advanceTimersByTime(1000); });
  expect(mocked.invoke.mock.calls.length).toBeGreaterThan(calls);
});

it('explains persistent account access before continuing a cloud sign-in', async () => {
  const user = userEvent.setup();
  current = { ...initial, kind: 'cloud_login', needsDirectory: false };
  render(<AgentInteractionWindow />);
  expect(await screen.findByText(/无需逐个目录授权/)).toBeVisible();
  expect(screen.getByText(/可在设置中撤销/)).toBeVisible();
  expect(screen.getByText(/不要把密码/)).toBeVisible();
  await user.click(screen.getByRole('button', { name: '继续登录' }));
  expect(mocked.invoke).toHaveBeenCalledWith('agent_interaction_decide', { interactionId: initial.id, allow: true });
});

it('requires the missing batch folder even when another item already has a folder', async () => {
  current = { ...initial, downloadDirectories: ['C:/Already selected'], needsDirectory: true };
  render(<AgentInteractionWindow />);
  expect(await screen.findByText('C:/Already selected')).toBeVisible();
  const allow = screen.getByRole('button', { name: '允许' });
  expect(allow).toBeDisabled();
  expect(screen.getByRole('button', { name: '选择下载目录' })).toBeEnabled();
  fireEvent.click(allow);
  expect(mocked.invoke.mock.calls.filter(([name]) => name === 'agent_interaction_decide')).toHaveLength(0);
});
