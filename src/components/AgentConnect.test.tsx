import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, afterEach, vi } from 'vitest';
import { AgentConnect, type AgentConnectionInfo } from './AgentConnect';

const nativeWriteText = vi.hoisted(() => vi.fn());
vi.mock('@tauri-apps/plugin-clipboard-manager', () => ({ writeText: nativeWriteText }));

const info: AgentConnectionInfo = {
  executablePath: 'C:/VD 中文 space/vd-mcp-server.exe',
  executableExists: true,
  configJson: JSON.stringify({ mcpServers: { versadown: { command: 'C:/VD 中文 space/vd-mcp-server.exe', args: ['--lang', 'zh-CN'] } } }),
  installationPrompt: '请按当前客户端方式接入 VD，保留其他 MCP，然后实际调用 vd_health。',
};
let clipboard: ReturnType<typeof vi.fn>;

beforeEach(() => {
  clipboard = vi.fn().mockResolvedValue(undefined);
  nativeWriteText.mockReset().mockResolvedValue(undefined);
  Object.defineProperty(navigator, 'clipboard', { configurable: true, value: { writeText: clipboard } });
});
afterEach(() => {
  Reflect.deleteProperty(window, '__TAURI_INTERNALS__');
});

it('offers JSON as the primary copy action and keeps the AI setup prompt available', async () => {
  const user = userEvent.setup();
  // userEvent installs its own clipboard stub, so install the application spy afterwards.
  Object.defineProperty(navigator, 'clipboard', { configurable: true, value: { writeText: clipboard } });
  const load = vi.fn().mockResolvedValue(info);
  render(<AgentConnect loadConnectionInfo={load} />);
  const jsonButton = screen.getByRole('button', { name: '复制 MCP JSON' });
  const promptButton = screen.getByRole('button', { name: '复制 AI 接入提示词' });
  expect(screen.getAllByRole('button')).toEqual([jsonButton, promptButton]);
  expect(jsonButton).toHaveClass('primary');
  expect(promptButton).toHaveClass('secondary');
  expect(jsonButton).toBeDisabled();
  await waitFor(() => expect(jsonButton).toBeEnabled());
  expect(load).toHaveBeenCalledWith('zh-CN');
  expect(screen.getByText(info.executablePath)).toBeVisible();
  await user.click(jsonButton);
  expect(clipboard).toHaveBeenLastCalledWith(info.configJson);
  expect(screen.getByRole('status')).toHaveTextContent('MCP JSON 已复制');
  await user.click(promptButton);
  expect(clipboard).toHaveBeenLastCalledWith(info.installationPrompt);
  expect(screen.getByRole('status')).toHaveTextContent('接入提示词已复制');
});

it('uses the native clipboard in VD and reports a failed copy before allowing retry', async () => {
  const user = userEvent.setup();
  Object.defineProperty(window, '__TAURI_INTERNALS__', { configurable: true, value: {} });
  nativeWriteText.mockRejectedValueOnce(new Error('clipboard denied'));
  render(<AgentConnect loadConnectionInfo={vi.fn().mockResolvedValue(info)} />);
  const button = screen.getByRole('button', { name: '复制 AI 接入提示词' });
  await waitFor(() => expect(button).toBeEnabled());
  await user.click(button);
  expect(await screen.findByRole('alert')).toHaveTextContent('复制失败');
  expect(screen.queryByText(/接入提示词已复制/)).not.toBeInTheDocument();
  expect(button).toBeEnabled();
  await user.click(button);
  expect(screen.getByRole('status')).toHaveTextContent('接入提示词已复制');
  expect(nativeWriteText).toHaveBeenCalledTimes(2);
  expect(clipboard).not.toHaveBeenCalled();
});

it('shows a missing companion program and rechecks before enabling setup copying', async () => {
  const user = userEvent.setup();
  const load = vi.fn().mockResolvedValueOnce({ ...info, executableExists: false }).mockResolvedValue(info);
  render(<AgentConnect loadConnectionInfo={load} />);
  expect(await screen.findByRole('alert')).toHaveTextContent('未找到配套 vd-mcp-server');
  expect(screen.getByText(info.executablePath)).toBeVisible();
  expect(screen.getByRole('button', { name: '复制 AI 接入提示词' })).toBeDisabled();
  expect(screen.getByRole('button', { name: '复制 MCP JSON' })).toBeDisabled();
  await user.click(screen.getByRole('button', { name: '重新检查' }));
  await waitFor(() => expect(screen.getByRole('button', { name: '复制 MCP JSON' })).toBeEnabled());
  expect(screen.queryByRole('alert')).not.toBeInTheDocument();
});

it('keeps copying disabled when loading fails and lets the user retry', async () => {
  const user = userEvent.setup();
  const load = vi.fn().mockRejectedValueOnce(new Error('private backend diagnostic')).mockResolvedValue(info);
  render(<AgentConnect loadConnectionInfo={load} />);
  expect(await screen.findByRole('alert')).toHaveTextContent('无法读取接入信息');
  expect(screen.queryByText('private backend diagnostic')).not.toBeInTheDocument();
  expect(screen.getByRole('button', { name: '复制 MCP JSON' })).toBeDisabled();
  await user.click(screen.getByRole('button', { name: '重新检查' }));
  await waitFor(() => expect(screen.getByRole('button', { name: '复制 MCP JSON' })).toBeEnabled());
});
