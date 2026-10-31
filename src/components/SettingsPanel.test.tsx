import { readFileSync } from 'node:fs';
import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { vi } from 'vitest';
import { SettingsPanel } from './SettingsPanel';
import { defaultSettings } from '../lib/api';

const initial = {
  downloadDirectory: 'C:\\Users\\tester\\Downloads',
  launcherVisibility: 'always' as const,
  proxy: { enabled: false, protocol: 'http' as const, host: '', port: 7890, username: '', password: '' },
  ai: { enabled: false, baseUrl: '', model: '', apiKeyConfigured: false },
  magnetMetadata: { enabled: true, baseUrl: 'http://peer.aigutta.com:8080' },
  aria2: { rpcPort: 6800, rpcToken: '', maxConcurrentDownloads: 20, maxConnectionsPerServer: 4, btSeedRatio: 1.5, debugLogging: false, overallDownloadLimit: 0, p2pUploadLimit: 1048576 },
  deleteTransferredShareFilesAfterDownload: true,
  cloudUploadLimit: 0
};

it('changes the interface theme from common settings', async () => {
  const user = userEvent.setup();
  render(<SettingsPanel initial={initial} onChange={() => {}} />);

  expect(screen.getAllByRole('radio', { name: /主题/ })).toHaveLength(8);
  await user.click(screen.getByRole('radio', { name: '暖阳金主题' }));

  expect(document.documentElement).toHaveAttribute('data-theme', 'warm-gold');
  expect(screen.getByRole('radio', { name: '暖阳金主题' })).toHaveAttribute('aria-checked', 'true');
});

it('separates settings into common, network, and advanced workspaces', async () => {
  const user = userEvent.setup();
  render(<SettingsPanel initial={initial} onChange={() => {}} />);

  expect(screen.getByRole('tab', { name: '通用' })).toBeVisible();
  expect(screen.getByRole('tab', { name: '网络' })).toBeVisible();
  expect(screen.getByRole('tab', { name: '高级' })).toBeVisible();
  expect(screen.getByRole('textbox', { name: '下载目录' })).toBeVisible();

  await user.click(screen.getByRole('tab', { name: '网络' }));
  expect(screen.getByRole('checkbox', { name: '启用下载代理' })).toBeVisible();
  expect(screen.queryByRole('textbox', { name: '下载目录' })).not.toBeInTheDocument();

  await user.click(screen.getByRole('tab', { name: '高级' }));
  expect(screen.queryByRole('checkbox', { name: '启用调试报告' })).not.toBeInTheDocument();
  expect(screen.queryByText('隐私规则')).not.toBeInTheDocument();
  expect(screen.getByRole('checkbox', { name: '打开日志调试' })).toBeVisible();
  expect(screen.getByRole('checkbox', { name: '开启隐私下载' })).toBeVisible();
  expect(screen.queryByRole('checkbox', { name: '启用磁链元数据服务' })).not.toBeInTheDocument();
});

it('keeps proxy, traffic caps, and aria2 configuration in separate network settings groups', async () => {
  const user = userEvent.setup();
  render(<SettingsPanel initial={initial} onChange={() => {}} />);
  await user.click(screen.getByRole('tab', { name: '网络' }));

  expect(screen.getByText('启用下载代理服务器')).toBeVisible();
  const rateLimitForm = screen.getByText('限速设置').closest('form');
  const aria2Form = screen.getByText('aria2 下载引擎').closest('form');
  const overallDownloadLimit = screen.getByRole('spinbutton', { name: '整体下载限速（MiB/s）' });
  const p2pUploadLimit = screen.getByRole('spinbutton', { name: 'P2P 上传限速（MiB/s）' });
  const cloudUploadLimit = screen.getByRole('spinbutton', { name: '网盘上传限速（MiB/s）' });
  const rateLimitFieldset = overallDownloadLimit.closest('fieldset');

  expect(rateLimitForm).toContainElement(overallDownloadLimit);
  expect(rateLimitForm).toHaveTextContent('限速设置（0为不限速）');
  expect(rateLimitForm).toContainElement(p2pUploadLimit);
  expect(rateLimitForm).toContainElement(cloudUploadLimit);
  expect(p2pUploadLimit.closest('fieldset')).toBe(rateLimitFieldset);
  expect(cloudUploadLimit.closest('fieldset')).toBe(rateLimitFieldset);
  expect(screen.getByRole('button', { name: '保存限速设置' }).parentElement).toBe(rateLimitFieldset);
  expect(aria2Form).not.toContainElement(overallDownloadLimit);
  expect(aria2Form).not.toContainElement(p2pUploadLimit);
  expect(aria2Form).not.toContainElement(cloudUploadLimit);
});

it('keeps rate limits and their save control inline while accepting one decimal place', async () => {
  const user = userEvent.setup();
  const onChange = vi.fn();
  const stylesheet = document.createElement('style');
  stylesheet.textContent = readFileSync('src/styles.css', 'utf8');
  document.head.appendChild(stylesheet);
  try {
    render(<SettingsPanel initial={initial} onChange={onChange} />);
    await user.click(screen.getByRole('tab', { name: '网络' }));

    const overallDownloadLimit = screen.getByRole('spinbutton', { name: '整体下载限速（MiB/s）' });
    const p2pUploadLimit = screen.getByRole('spinbutton', { name: 'P2P 上传限速（MiB/s）' });
    const cloudUploadLimit = screen.getByRole('spinbutton', { name: '网盘上传限速（MiB/s）' });
    const rateLimitFieldset = overallDownloadLimit.closest('fieldset')!;
    expect(screen.getByRole('button', { name: '保存限速设置' }).parentElement).toBe(rateLimitFieldset);
    expect(getComputedStyle(rateLimitFieldset).gridTemplateColumns).toBe('repeat(3, minmax(0, 1fr)) auto');
    expect(overallDownloadLimit).toHaveAttribute('step', '0.1');
    expect(p2pUploadLimit.value).toBe('1.0');
    expect(cloudUploadLimit.value).toBe('0.0');

    await user.clear(overallDownloadLimit);
    await user.type(overallDownloadLimit, '2.56');
    expect(overallDownloadLimit.value).toBe('2.5');
    await user.click(screen.getByRole('button', { name: '保存限速设置' }));

    expect(onChange).toHaveBeenLastCalledWith({
      ...initial,
      aria2: { ...initial.aria2, overallDownloadLimit: 2_621_440 },
      cloudUploadLimit: 0
    });
  } finally {
    stylesheet.remove();
  }
});

it('shows the rate-limit helper and the privacy and aria2 diagnostic descriptions', async () => {
  const user = userEvent.setup();
  const { container } = render(<SettingsPanel initial={initial} onChange={() => {}} />);

  expect(container.querySelectorAll('.settings-content small')).toHaveLength(0);
  await user.click(screen.getByRole('tab', { name: '网络' }));
  expect(container.querySelectorAll('.settings-content small')).toHaveLength(1);
  expect(screen.getByText('（0为不限速）')).toBeVisible();
  await user.click(screen.getByRole('tab', { name: '高级' }));
  const descriptions = Array.from(container.querySelectorAll('.settings-content small'));
  expect(descriptions).toHaveLength(2);
  expect(descriptions[0]).toHaveTextContent('保存 aria2 引擎和本地 RPC 调试日志');
  expect(descriptions[1]).toHaveTextContent('私密任务只会在本次应用会话解锁后显示；下载文件本身不会加密。');
});

it('saves a custom directory on blur without a separate save button', async () => {
  const user = userEvent.setup();
  const onChange = vi.fn();
  render(<SettingsPanel initial={initial} onChange={onChange} />);

  const directory = screen.getByRole('textbox', { name: '下载目录' });
  await user.clear(directory);
  await user.type(directory, 'D:\\Downloads\\Games');
  await user.tab();

  expect(onChange).toHaveBeenLastCalledWith({ ...initial, downloadDirectory: 'D:\\Downloads\\Games' });
  expect(screen.queryByRole('button', { name: '保存下载目录' })).not.toBeInTheDocument();
});

it('applies the download directory immediately after native folder selection', async () => {
  const user = userEvent.setup();
  const onChange = vi.fn();
  render(<SettingsPanel initial={initial} onChange={onChange} browseDirectory={async () => 'E:\\Library'} />);

  await user.click(screen.getByRole('button', { name: '浏览' }));

  expect(screen.getByRole('textbox', { name: '下载目录' })).toHaveValue('E:\\Library');
  expect(onChange).toHaveBeenLastCalledWith({ ...initial, downloadDirectory: 'E:\\Library' });
});

it('offers an OpenAI-compatible AI analysis configuration in advanced settings', async () => {
  const user = userEvent.setup();
  render(<SettingsPanel initial={initial} onChange={() => {}} />);

  await user.click(screen.getByRole('tab', { name: '高级' }));

  expect(screen.getByRole('checkbox', { name: '启用 AI 智能分析' })).toBeVisible();
  expect(screen.getByRole('textbox', { name: '服务地址' })).toBeVisible();
  expect(screen.getByLabelText('API Key')).toHaveAttribute('type', 'password');
  expect(screen.getByRole('combobox', { name: '模型' })).toBeVisible();
  expect(screen.queryByRole('button', { name: '获取可用模型' })).not.toBeInTheDocument();
  expect(screen.queryByRole('button', { name: '保存 AI 配置' })).not.toBeInTheDocument();
});

it('lets network settings save buttons wrap long translated labels', async () => {
  const user = userEvent.setup();
  const stylesheet = document.createElement('style');
  stylesheet.textContent = readFileSync('src/styles.css', 'utf8');
  document.head.appendChild(stylesheet);
  try {
    const { container } = render(<SettingsPanel initial={initial} onChange={() => {}} />);
    await user.click(screen.getAllByRole('tab')[1]);

    const saveButtons = container.querySelectorAll<HTMLButtonElement>('.settings-inline-save');
    expect(saveButtons).toHaveLength(3);
    for (const button of saveButtons) {
      expect(getComputedStyle(button).whiteSpace).toBe('normal');
      expect(getComputedStyle(button).height).toBe('auto');
      expect(getComputedStyle(button).minHeight).toBe('38px');
    }
  } finally {
    stylesheet.remove();
  }
});

it('allows the privacy download description to wrap inside its settings card', async () => {
  const user = userEvent.setup();
  const stylesheet = document.createElement('style');
  stylesheet.textContent = readFileSync('src/styles.css', 'utf8');
  document.head.appendChild(stylesheet);
  try {
    const { container } = render(<SettingsPanel initial={initial} onChange={() => {}} />);
    await user.click(screen.getAllByRole('tab')[2]);

    const description = container.querySelector<HTMLElement>('.privacy-download-settings .setting-row small')!;
    expect(description).toBeVisible();
    expect(getComputedStyle(description).whiteSpace).toBe('normal');
    expect(getComputedStyle(description).maxWidth).toBe('480px');
  } finally {
    stylesheet.remove();
  }
});

it('saves the download status notification preference from common settings', async () => {
  const user = userEvent.setup();
  const onChange = vi.fn();
  render(<SettingsPanel initial={{ ...initial, downloadStatusNotifications: true }} onChange={onChange} />);

  const notificationToggle = screen.getByRole('checkbox', { name: '下载状态变动通知' });
  expect(notificationToggle).toBeChecked();
  await user.click(notificationToggle);

  expect(onChange).toHaveBeenCalledWith(expect.objectContaining({ downloadStatusNotifications: false }));
  expect(notificationToggle).not.toBeChecked();
});

it('turns browser link association off and back on from common settings', async () => {
  const user = userEvent.setup();
  const onChange = vi.fn().mockResolvedValue(undefined);
  render(<SettingsPanel initial={{ ...initial, associateDownloadLinks: true }} onChange={onChange} />);

  const toggle = screen.getByRole('checkbox', { name: '关联磁力、迅雷和电驴链接' });
  expect(toggle).toBeChecked();
  await user.click(toggle);

  expect(onChange).toHaveBeenLastCalledWith(expect.objectContaining({ associateDownloadLinks: false }));
  expect(toggle).not.toBeChecked();
  await user.click(toggle);
  expect(onChange).toHaveBeenLastCalledWith(expect.objectContaining({ associateDownloadLinks: true }));
});

it('restores the link association toggle and reports why it could not be changed', async () => {
  const user = userEvent.setup();
  const onNotice = vi.fn();
  const onChange = vi.fn().mockRejectedValue(new Error('无法设置默认程序：xdg-mime 退出码 1'));
  render(<SettingsPanel initial={{ ...initial, associateDownloadLinks: false }} onChange={onChange} onNotice={onNotice} />);

  const toggle = screen.getByRole('checkbox', { name: '关联磁力、迅雷和电驴链接' });
  await user.click(toggle);

  await waitFor(() => expect(toggle).not.toBeChecked());
  expect(onNotice).toHaveBeenCalledWith({ kind: 'error', message: '无法设置默认程序：xdg-mime 退出码 1' });
});

it('refreshes models when its selector is opened and saves the complete AI configuration on selection', async () => {
  const user = userEvent.setup();
  const loadAiModels = vi.fn().mockResolvedValue(['gpt-4.1-mini', 'gpt-5-mini']);
  const saveAiSettings = vi.fn().mockResolvedValue({ enabled: true, baseUrl: 'https://gateway.example.test/v1', model: 'gpt-5-mini', apiKeyConfigured: true });
  render(<SettingsPanel initial={initial} onChange={() => {}} loadAiModels={loadAiModels} saveAiSettings={saveAiSettings} />);

  await user.click(screen.getByRole('tab', { name: '高级' }));
  await user.click(screen.getByRole('checkbox', { name: '启用 AI 智能分析' }));
  await user.type(screen.getByRole('textbox', { name: '服务地址' }), 'https://gateway.example.test/v1');
  await user.type(screen.getByLabelText('API Key'), 'sk-local-secret');
  const model = screen.getByRole('combobox', { name: '模型' });
  fireEvent.pointerDown(model);
  expect(await screen.findByRole('option', { name: 'gpt-5-mini' })).toBeVisible();
  expect(loadAiModels).toHaveBeenCalledWith({ enabled: true, baseUrl: 'https://gateway.example.test/v1', model: '', apiKey: 'sk-local-secret' });

  await user.selectOptions(model, 'gpt-5-mini');
  expect(saveAiSettings).toHaveBeenCalledWith({ enabled: true, baseUrl: 'https://gateway.example.test/v1', model: 'gpt-5-mini', apiKey: 'sk-local-secret' });
});

it('shows the returned model-list error when the selector cannot load models', async () => {
  const user = userEvent.setup();
  const onNotice = vi.fn();
  render(<SettingsPanel initial={initial} onChange={() => {}} loadAiModels={vi.fn().mockRejectedValue(new Error('AI 服务返回 HTTP 401：invalid api key'))} onNotice={onNotice} />);

  await user.click(screen.getByRole('tab', { name: '高级' }));
  await user.type(screen.getByRole('textbox', { name: '服务地址' }), 'https://gateway.example.test/v1');
  await user.type(screen.getByLabelText('API Key'), 'bad-key');
  const model = screen.getByRole('combobox', { name: '模型' });
  await user.click(model);

  const error = await screen.findByRole('status');
  expect(error).toHaveTextContent('无法获取模型列表，可能是服务地址或 API Key 不正确：AI 服务返回 HTTP 401：invalid api key');
  expect(error.closest('.ai-settings')).not.toBeNull();
  expect(error.compareDocumentPosition(model) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  expect(model).not.toHaveFocus();
  expect(onNotice).not.toHaveBeenCalled();
});

it('enables cleanup for application-created transfer files by default', () => {
  render(<SettingsPanel initial={defaultSettings} onChange={() => {}} />);

  expect(screen.getByLabelText('下载完成后自动删除转存文件')).toBeChecked();
  expect(screen.queryByText('仅删除本应用为下载创建的转存文件，不会删除您的其他网盘文件。')).not.toBeInTheDocument();
});

it('defaults to twenty concurrent tasks and four connections while preserving an explicit eight', async () => {
  const user = userEvent.setup();
  expect(defaultSettings.aria2.maxConnectionsPerServer).toBe(4);
  expect(defaultSettings.aria2.maxConcurrentDownloads).toBe(20);

  render(<SettingsPanel initial={{ ...defaultSettings, aria2: { ...defaultSettings.aria2, maxConnectionsPerServer: 8 } }} onChange={() => {}} />);
  await user.click(screen.getByRole('tab', { name: '网络' }));

  expect(screen.getByRole('spinbutton', { name: '单任务并发连接数' })).toHaveValue(8);
});

it('asks for and confirms a new password when privacy has no existing password', async () => {
  const user = userEvent.setup();
  const configurePrivacyDownload = vi.fn().mockResolvedValue({ ...initial, privacyDownloadEnabled: true, privacyPasswordConfigured: true });
  render(<SettingsPanel initial={initial} onChange={() => {}} configurePrivacyDownload={configurePrivacyDownload} />);

  await user.click(screen.getByRole('tab', { name: '高级' }));
  await user.click(screen.getByRole('checkbox', { name: '开启隐私下载' }));
  await user.type(screen.getByLabelText('设置新密码'), 'private-pass');
  await user.type(screen.getByLabelText('确认新密码'), 'private-pass');
  await user.click(screen.getByRole('button', { name: '保存密码' }));

  expect(configurePrivacyDownload).toHaveBeenCalledWith(true, undefined, 'private-pass');
  expect(screen.queryByRole('status')).not.toBeInTheDocument();
});

it('requires the old password before replacing an existing privacy password', async () => {
  const user = userEvent.setup();
  const configurePrivacyDownload = vi.fn().mockResolvedValue({ ...initial, privacyDownloadEnabled: true, privacyPasswordConfigured: true });
  render(<SettingsPanel initial={{ ...initial, privacyDownloadEnabled: true, privacyPasswordConfigured: true }} onChange={() => {}} configurePrivacyDownload={configurePrivacyDownload} />);

  await user.click(screen.getByRole('tab', { name: '高级' }));
  await user.type(screen.getByLabelText('输入旧密码'), 'old-pass');
  await user.type(screen.getByLabelText('设置新密码'), 'new-pass');
  await user.click(screen.getByRole('button', { name: '保存密码' }));

  expect(screen.queryByLabelText('确认新密码')).not.toBeInTheDocument();
  expect(configurePrivacyDownload).toHaveBeenCalledWith(true, 'old-pass', 'new-pass');
});

it('keeps privacy password controls disabled after debug logging refreshes a configured password', async () => {
  const user = userEvent.setup();
  const { rerender } = render(<SettingsPanel initial={{ ...initial, privacyDownloadEnabled: false, privacyPasswordConfigured: false }} onChange={() => {}} />);

  await user.click(screen.getByRole('tab', { name: '高级' }));
  await user.click(screen.getByRole('checkbox', { name: '打开日志调试' }));
  rerender(<SettingsPanel initial={{ ...initial, aria2: { ...initial.aria2, debugLogging: true }, privacyDownloadEnabled: false, privacyPasswordConfigured: true }} onChange={() => {}} />);

  expect(screen.getByLabelText('输入旧密码')).toBeDisabled();
  expect(screen.getByLabelText('设置新密码')).toBeDisabled();
  expect(screen.getByRole('button', { name: '保存密码' })).toBeDisabled();
});

it('uses compact aligned controls and consistent cards across settings tabs', async () => {
  const user = userEvent.setup();
  const stylesheet = document.createElement('style');
  stylesheet.textContent = readFileSync('src/styles.css', 'utf8');
  document.head.appendChild(stylesheet);
  try {
    render(<SettingsPanel initial={{ ...initial, privacyDownloadEnabled: true, privacyPasswordConfigured: true }} onChange={() => {}} />);

    const cleanupToggle = screen.getByRole('checkbox', { name: '下载完成后自动删除转存文件' });
    expect(getComputedStyle(cleanupToggle).width).toBe('18px');

    const theme = screen.getByRole('region', { name: '界面主题' });
    const directory = screen.getByRole('textbox', { name: '下载目录' }).closest('form');
    expect(Number.parseFloat(getComputedStyle(directory!).marginTop)).toBeGreaterThan(0);
    expect(getComputedStyle(theme).borderRadius).toBe(getComputedStyle(directory!).borderRadius);

    await user.click(screen.getByRole('tab', { name: '网络' }));
    expect(getComputedStyle(screen.getByRole('checkbox', { name: '启用下载代理' })).width).toBe('18px');
    const proxyFieldset = screen.getByLabelText('密码（可选）').closest('fieldset');
    expect(screen.getByRole('button', { name: '保存代理设置' }).parentElement).toBe(proxyFieldset);
    expect(Array.from(proxyFieldset!.children).slice(-3)).toEqual([
      screen.getByRole('textbox', { name: '用户名（可选）' }).closest('label'),
      screen.getByLabelText('密码（可选）').closest('label'),
      screen.getByRole('button', { name: '保存代理设置' }),
    ]);
    const aria2Fieldset = screen.getByRole('spinbutton', { name: 'BT 分享率' }).closest('fieldset');
    expect(screen.getByRole('button', { name: '保存 aria2 设置' }).parentElement).toBe(aria2Fieldset);
    expect(getComputedStyle(aria2Fieldset!).gridTemplateColumns).toBe('repeat(3,minmax(0,1fr)) auto');
    expect(Array.from(aria2Fieldset!.children).slice(-2)).toEqual([
      screen.getByRole('spinbutton', { name: 'BT 分享率' }).closest('label'),
      screen.getByRole('button', { name: '保存 aria2 设置' }),
    ]);

    await user.click(screen.getByRole('tab', { name: '高级' }));
    const privacyToggle = screen.getByRole('checkbox', { name: '开启隐私下载' });
    expect(getComputedStyle(privacyToggle).width).toBe('18px');
    expect(getComputedStyle(privacyToggle).height).toBe('18px');
    const privacyDescription = screen.getByText(/私密任务只会/);
    expect(getComputedStyle(privacyDescription).whiteSpace).toBe('normal');
    expect(getComputedStyle(privacyDescription).maxWidth).toBe('480px');
    const oldPassword = screen.getByLabelText('输入旧密码');
    const newPassword = screen.getByLabelText('设置新密码');
    const savePassword = screen.getByRole('button', { name: '保存密码' });
    expect(oldPassword.parentElement?.parentElement).toBe(newPassword.parentElement?.parentElement);
    expect(savePassword.parentElement).toBe(oldPassword.parentElement?.parentElement);
    expect(getComputedStyle(oldPassword).width).toBe(getComputedStyle(newPassword).width);
  } finally {
    stylesheet.remove();
  }
});

it('uses the shared settings form treatment for privacy password inputs', async () => {
  const user = userEvent.setup();
  render(<SettingsPanel initial={{ ...initial, privacyDownloadEnabled: true, privacyPasswordConfigured: true }} onChange={() => {}} />);

  await user.click(screen.getByRole('tab', { name: '高级' }));
  expect(screen.getByLabelText('输入旧密码').closest('form')).toHaveClass('proxy-settings');
  expect(screen.getByLabelText('设置新密码').closest('form')).toHaveClass('proxy-settings');
});

it('does not expose internal magnet metadata service settings', async () => {
  const user = userEvent.setup();
  render(<SettingsPanel initial={initial} onChange={() => {}} />);

  await user.click(screen.getByRole('tab', { name: '高级' }));
  expect(screen.queryByRole('checkbox', { name: '启用磁链元数据服务' })).not.toBeInTheDocument();
  expect(screen.queryByRole('textbox', { name: '磁链元数据服务地址' })).not.toBeInTheDocument();
  expect(screen.queryByRole('button', { name: '保存磁链服务' })).not.toBeInTheDocument();
});

it('lets users restore a hidden floating ball from the main settings', async () => {
  const user = userEvent.setup();
  const onChange = vi.fn();
  render(<SettingsPanel initial={{ ...initial, launcherVisibility: 'hidden' }} onChange={onChange} />);

  await user.selectOptions(screen.getByRole('combobox', { name: '悬浮球显示' }), 'always');

  expect(onChange).toHaveBeenLastCalledWith({ ...initial, launcherVisibility: 'always' });
});

it('saves an authenticated HTTPS proxy for aria2 downloads', async () => {
  const user = userEvent.setup();
  const onChange = vi.fn();
  render(<SettingsPanel initial={initial} onChange={onChange} />);
  await user.click(screen.getByRole('tab', { name: '网络' }));

  await user.click(screen.getByRole('checkbox', { name: '启用下载代理' }));
  await user.selectOptions(screen.getByRole('combobox', { name: '协议' }), 'https');
  await user.clear(screen.getByRole('textbox', { name: '服务器' }));
  await user.type(screen.getByRole('textbox', { name: '服务器' }), '127.0.0.1');
  await user.clear(screen.getByRole('spinbutton', { name: '端口' }));
  await user.type(screen.getByRole('spinbutton', { name: '端口' }), '1080');
  await user.type(screen.getByRole('textbox', { name: '用户名（可选）' }), 'alice');
  await user.type(screen.getByLabelText('密码（可选）'), 'secret');
  await user.click(screen.getByRole('button', { name: '保存代理设置' }));

  expect(onChange).toHaveBeenLastCalledWith({
    downloadDirectory: 'C:\\Users\\tester\\Downloads',
    launcherVisibility: 'always',
    proxy: { enabled: true, protocol: 'https', host: '127.0.0.1', port: 1080, username: 'alice', password: 'secret' },
    ai: { enabled: false, baseUrl: '', model: '', apiKeyConfigured: false },
    magnetMetadata: { enabled: true, baseUrl: 'http://peer.aigutta.com:8080' },
    aria2: { rpcPort: 6800, rpcToken: '', maxConcurrentDownloads: 20, maxConnectionsPerServer: 4, btSeedRatio: 1.5, debugLogging: false, overallDownloadLimit: 0, p2pUploadLimit: 1048576 },
    deleteTransferredShareFilesAfterDownload: true,
    cloudUploadLimit: 0
  });
});

it('saves aria2 RPC and engine settings from the network settings', async () => {
  const user = userEvent.setup();
  const onChange = vi.fn();
  render(<SettingsPanel initial={initial} onChange={onChange} />);
  await user.click(screen.getByRole('tab', { name: '网络' }));

  await user.clear(screen.getByRole('spinbutton', { name: 'RPC 端口' }));
  await user.type(screen.getByRole('spinbutton', { name: 'RPC 端口' }), '16800');
  await user.type(screen.getByLabelText('aria2 RPC Token'), 'local-token');
  await user.clear(screen.getByRole('spinbutton', { name: '最大并发任务数' }));
  await user.type(screen.getByRole('spinbutton', { name: '最大并发任务数' }), '3');
  await user.clear(screen.getByRole('spinbutton', { name: '单任务并发连接数' }));
  await user.type(screen.getByRole('spinbutton', { name: '单任务并发连接数' }), '4');
  await user.clear(screen.getByRole('spinbutton', { name: 'BT 分享率' }));
  await user.type(screen.getByRole('spinbutton', { name: 'BT 分享率' }), '1.5');
  await user.click(screen.getByRole('button', { name: '保存 aria2 设置' }));

  expect(onChange).toHaveBeenLastCalledWith({
    ...initial,
    aria2: {
      rpcPort: 16800,
      rpcToken: 'local-token',
      maxConcurrentDownloads: 3,
      maxConnectionsPerServer: 4,
      btSeedRatio: 1.5,
      debugLogging: false,
      overallDownloadLimit: 0,
      p2pUploadLimit: 1048576
    }
  });
});

it('toggles visibility for the optional aria2 RPC Token', async () => {
  const user = userEvent.setup();
  render(<SettingsPanel initial={{ ...initial, aria2: { ...initial.aria2, rpcToken: 'local-token' } }} onChange={vi.fn()} />);
  await user.click(screen.getByRole('tab', { name: '网络' }));

  const token = screen.getByLabelText('aria2 RPC Token');
  expect(token).toHaveAttribute('type', 'password');
  await user.click(screen.getByRole('button', { name: '显示 RPC Token' }));
  expect(token).toHaveAttribute('type', 'text');
  await user.click(screen.getByRole('button', { name: '隐藏 RPC Token' }));
  expect(token).toHaveAttribute('type', 'password');
});

it('saves aria2 debug logging immediately from advanced settings', async () => {
  const user = userEvent.setup();
  const onChange = vi.fn();
  render(<SettingsPanel initial={initial} onChange={onChange} />);

  await user.click(screen.getByRole('tab', { name: '高级' }));
  await user.click(screen.getByRole('checkbox', { name: '打开日志调试' }));

  expect(onChange).toHaveBeenLastCalledWith({
    ...initial,
    aria2: { ...initial.aria2, debugLogging: true }
  });
  expect(screen.queryByRole('button', { name: '保存日志设置' })).not.toBeInTheDocument();
});

it('saves the standalone rate limits in bytes per second', async () => {
  const user = userEvent.setup();
  const onChange = vi.fn();
  render(<SettingsPanel initial={initial} onChange={onChange} />);
  await user.click(screen.getByRole('tab', { name: '网络' }));

  await user.clear(screen.getByRole('spinbutton', { name: '整体下载限速（MiB/s）' }));
  await user.type(screen.getByRole('spinbutton', { name: '整体下载限速（MiB/s）' }), '2.5');
  await user.clear(screen.getByRole('spinbutton', { name: 'P2P 上传限速（MiB/s）' }));
  await user.type(screen.getByRole('spinbutton', { name: 'P2P 上传限速（MiB/s）' }), '1.5');
  await user.clear(screen.getByRole('spinbutton', { name: '网盘上传限速（MiB/s）' }));
  await user.type(screen.getByRole('spinbutton', { name: '网盘上传限速（MiB/s）' }), '3');
  await user.click(screen.getByRole('button', { name: '保存限速设置' }));

  expect(onChange).toHaveBeenLastCalledWith({
    ...initial,
    aria2: {
      ...initial.aria2,
      overallDownloadLimit: 2_621_440,
      p2pUploadLimit: 1_572_864
    },
    cloudUploadLimit: 3_145_728
  });
});

it('shows the native aria2 error instead of claiming a failed configuration was applied', async () => {
  const user = userEvent.setup();
  const onChange = vi.fn().mockRejectedValue(new Error('aria2 RPC 端口 16800 已被占用'));
  render(<SettingsPanel initial={{ ...initial, aria2: { ...initial.aria2, rpcToken: 'local-token' } }} onChange={onChange} />);
  await user.click(screen.getByRole('tab', { name: '网络' }));
  await user.click(screen.getByRole('button', { name: '保存 aria2 设置' }));

  expect(await screen.findByRole('status')).toHaveTextContent('aria2 RPC 端口 16800 已被占用');
  expect(screen.queryByText('已保存并立即应用')).not.toBeInTheDocument();
});
