import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { vi } from 'vitest';
import { AgentCloudGrants } from './AgentCloudGrants';
import type { CloudAccount, ProviderDescriptor } from '../lib/cloudTypes';

const provider: ProviderDescriptor = {
  provider: 'quark', displayName: '夸克', shareDisplayName: '夸克分享', uploadDisplayName: '夸克', shareDirectoryName: '分享',
  rootDirectoryId: '0', recycleBinDirectoryId: 'trash', authorization: { provider: 'quark', web: true, password: false, passwordLoginMode: 'none' },
  authorizationUrl: 'https://pan.quark.cn', passwordLoginHosts: [], shareHosts: [], sharePatterns: [], maxUploadConcurrency: 2,
  downloadConcurrency: { maxConnectionsPerFile: 4, maxConcurrentTasks: 4 },
  capabilities: { list: true, createDirectory: true, rename: true, move: true, copy: true, trash: true, delete: true, restore: true,
    download: true, upload: true, shareInspect: true, shareTransfer: true, recordedTransferResolve: true, shareCreate: true, shareList: true, shareCancel: true },
};
const account: CloudAccount = { provider: 'quark', id: 'private-account-id', displayName: 'My account', authMethod: 'web' };
function client() {
  return { getCloudProviderCatalog: vi.fn().mockResolvedValue([provider]), getCloudAccount: vi.fn().mockResolvedValue(account),
    listCloudDirectory: vi.fn().mockImplementation((_provider: string, id: string) => Promise.resolve({ id,
      items: id === '0' ? [{ id: 'work', parentId: '0', name: 'Work', kind: 'folder' }, { id: 'file', parentId: '0', name: 'File.txt', kind: 'file' }] : [] })) };
}

it('adds only the chosen signed-in account, folder and operations without saving automatically', async () => {
  const user = userEvent.setup();
  const api = client(); const change = vi.fn();
  render(<AgentCloudGrants grants={[]} onChange={change} client={api} />);
  expect(api.getCloudProviderCatalog).not.toHaveBeenCalled();
  await user.click(screen.getByRole('button', { name: '添加网盘授权' }));
  await screen.findByRole('option', { name: '夸克网盘 · My account' });
  await user.selectOptions(screen.getByRole('combobox', { name: '已登录的网盘账号' }), 'quark:private-account-id');
  await user.click(await screen.findByRole('button', { name: 'Work' }));
  await waitFor(() => expect(api.listCloudDirectory).toHaveBeenLastCalledWith('quark', 'work'));
  await waitFor(() => expect(screen.getByRole('button', { name: '添加当前目录授权' })).toBeEnabled());
  expect(screen.queryByRole('button', { name: 'File.txt' })).not.toBeInTheDocument();
  expect(screen.queryByRole('checkbox', { name: '转存分享' })).not.toBeInTheDocument();
  await user.click(screen.getByRole('checkbox', { name: '创建分享' }));
  await user.click(screen.getByRole('button', { name: '添加当前目录授权' }));
  expect(change).toHaveBeenCalledWith([{ provider: 'quark', accountId: 'private-account-id', directoryIds: ['work'], operations: ['browse', 'download', 'create_share'],
    accountLabel: 'My account', directoryLabels: { work: '根目录 / Work' } }]);
});

it('allows removing an expired account grant without loading accounts or directories', async () => {
  const user = userEvent.setup(); const api = client(); const change = vi.fn();
  render(<AgentCloudGrants grants={[{ provider: 'quark', accountId: 'private-account-id', directoryIds: ['work'], operations: ['upload'],
    accountLabel: 'My saved account', directoryLabels: { work: '根目录 / Projects' } }]} onChange={change} client={api} />);
  expect(screen.queryByText('private-account-id')).not.toBeInTheDocument();
  expect(screen.getByText('夸克网盘 · My saved account')).toBeVisible();
  expect(screen.getByText('根目录 / Projects · 上传')).toBeVisible();
  await user.click(screen.getByRole('button', { name: '移除第 1 项网盘授权' }));
  expect(change).toHaveBeenCalledWith([]);
  expect(api.getCloudAccount).not.toHaveBeenCalled();
  expect(api.listCloudDirectory).not.toHaveBeenCalled();
});

it('does not fabricate cloud access when there are no signed-in accounts', async () => {
  const user = userEvent.setup(); const api = client(); const change = vi.fn();
  api.getCloudAccount.mockRejectedValue({ code: 'not_authorized' });
  render(<AgentCloudGrants grants={[]} onChange={change} client={api} />);
  await user.click(screen.getByRole('button', { name: '添加网盘授权' }));
  await screen.findByText('没有可用的已登录账号，请先在网盘页面完成授权。');
  expect(screen.getByRole('button', { name: '添加当前目录授权' })).toBeDisabled();
  expect(change).not.toHaveBeenCalled();
});
