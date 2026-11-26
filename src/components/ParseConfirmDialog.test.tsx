import { render, screen, waitFor } from '@testing-library/react';
import { readFileSync } from 'node:fs';
import userEvent from '@testing-library/user-event';
import { ParseConfirmDialog } from './ParseConfirmDialog';
import { cloudCatalog } from '../test/cloudCatalog';

const cloudApi = vi.hoisted(() => ({ getCloudProviderCatalog: vi.fn() }));
vi.mock('../lib/cloudApi', () => cloudApi);

beforeEach(() => {
  cloudApi.getCloudProviderCatalog.mockReset();
  cloudApi.getCloudProviderCatalog.mockResolvedValue(cloudCatalog);
});

const result = {
  taskId: 'task-1', requiresConfirmation: true,
  resources: [
    { id: 'a', name: 'part-01.zip', size: '1.2 GB', selected: true },
    { id: 'b', name: 'part-02.zip', size: '1.2 GB', selected: true }
  ],
  processing: ['解压分卷并合并'],
  discoveredPassword: 'pass-123'
};

it('keeps discovered password masked until the user reveals it', async () => {
  const user = userEvent.setup();
  render(<ParseConfirmDialog result={result} onConfirm={() => {}} onSelectionChange={() => {}} onClose={() => {}} />);
  expect(screen.queryByText('pass-123')).not.toBeInTheDocument();
  await user.click(screen.getByRole('button', { name: /查看 后处理与密码/ }));
  expect(screen.queryByText('pass-123')).not.toBeInTheDocument();
  await user.click(screen.getByRole('button', { name: '显示' }));
  expect(screen.getByText('pass-123')).toBeVisible();
});

it('labels BitTorrent confirmation choices as select-file semantics', () => {
  render(<ParseConfirmDialog result={{ ...result, isBitTorrent: true }} onConfirm={() => {}} onSelectionChange={() => {}} onClose={() => {}} />);
  expect(screen.getByRole('heading', { name: '新建任务' })).toBeVisible();
  expect(screen.getAllByText('已选 2 / 2 个文件')).toHaveLength(2);
  expect(screen.getByRole('button', { name: /立即下载 2/ })).toBeVisible();
});

it('offers a private download action only while privacy download is enabled', async () => {
  const user = userEvent.setup();
  const onConfirm = vi.fn();
  render(<ParseConfirmDialog result={result} privacyEnabled onConfirm={onConfirm} onSelectionChange={() => {}} onClose={() => {}} />);

  expect(screen.getByRole('button', { name: /隐私下载 2/ })).toBeVisible();
  await user.click(screen.getByRole('button', { name: /隐私下载 2/ }));
  expect(onConfirm).toHaveBeenCalledWith(['a', 'b'], undefined, true);
});

it('allows a direct single-file confirmation without parsed file metadata', async () => {
  const user = userEvent.setup();
  const onConfirm = vi.fn();
  render(<ParseConfirmDialog
    privacyEnabled
    result={{ taskId: 'single-file', title: 'archive.zip', resources: [], processing: [] }}
    onConfirm={onConfirm}
    onSelectionChange={() => {}}
    onClose={() => {}}
  />);

  await user.click(screen.getByRole('button', { name: /隐私下载/ }));

  expect(onConfirm).toHaveBeenCalledWith([], undefined, true);
});

it('uses readable type for the confirmation download destination', () => {
  const stylesheet = document.createElement('style');
  stylesheet.textContent = readFileSync('src/styles.css', 'utf8');
  document.head.appendChild(stylesheet);
  try {
    render(<ParseConfirmDialog result={result} downloadDirectory="D:\\" onConfirm={() => {}} onSelectionChange={() => {}} onClose={() => {}} />);

    const destination = screen.getByRole('button', { name: '选择下载目录' });
    expect(getComputedStyle(destination).fontSize).toBe('14px');
    expect(getComputedStyle(destination.querySelector('strong')!).fontSize).toBe('14px');
    expect(getComputedStyle(destination.querySelector('i')!).fontSize).toBe('14px');
  } finally {
    stylesheet.remove();
  }
});

it('offers a compact file-selection toolbar with select-all controls', async () => {
  const user = userEvent.setup();
  const onSelectionChange = vi.fn();
  render(<ParseConfirmDialog result={{ ...result, isBitTorrent: true }} onConfirm={() => {}} onSelectionChange={onSelectionChange} onClose={() => {}} />);

  expect(screen.getByRole('checkbox', { name: '全选文件' })).toBeChecked();
  expect(screen.getAllByText('已选 2 / 2 个文件')).toHaveLength(2);

  await user.click(screen.getByRole('checkbox', { name: '全选文件' }));
  expect(onSelectionChange).toHaveBeenCalledWith([]);
});

it('keeps selection controls hidden while BitTorrent metadata is loading', () => {
  render(<ParseConfirmDialog result={{ ...result, resources: [], isBitTorrent: true }} onConfirm={() => {}} onSelectionChange={() => {}} onClose={() => {}} />);

  expect(screen.getByText('正在获取文件信息…')).toBeVisible();
  expect(screen.queryByRole('checkbox', { name: '全选文件' })).not.toBeInTheDocument();
});

it.each(['quark', 'pikpak', 'baidu', 'tianyi', 'aliyun', 'xunlei', 'uc'])('shows the %s share file list directly without a redundant summary', (provider) => {
  render(<ParseConfirmDialog
    result={{ ...result, title: '网盘分享文件', taskId: `cloud-share://${provider}/${'a'.repeat(64)}/${'b'.repeat(64)}` }}
    onConfirm={() => {}}
    onSelectionChange={() => {}}
    onClose={() => {}}
  />);

  expect(screen.queryByText('网盘分享文件')).not.toBeInTheDocument();
  expect(screen.queryByText('必要时会自动转存后下载')).not.toBeInTheDocument();
  expect(screen.getAllByText('已选 2 / 2 个文件')).toHaveLength(1);
  expect(screen.getByRole('checkbox', { name: '选择 part-01.zip' })).toBeChecked();
  expect(screen.queryByText('pass-123')).not.toBeInTheDocument();
});

it.each([{ processing: [] }, { processing: [' ', '\n'] }])('hides empty post-processing and password content: %j', ({ processing }) => {
  render(<ParseConfirmDialog result={{ ...result, processing, discoveredPassword: ' ' }} onConfirm={() => {}} onSelectionChange={() => {}} onClose={() => {}} />);
  expect(screen.queryByRole('button', { name: /后处理与密码/ })).not.toBeInTheDocument();
});

it('removes expanded processing content when the next result has none', async () => {
  const user = userEvent.setup();
  const props = { onConfirm: vi.fn(), onSelectionChange: vi.fn(), onClose: vi.fn() };
  const { rerender } = render(<ParseConfirmDialog result={result} {...props} />);
  await user.click(screen.getByRole('button', { name: /查看 后处理与密码/ }));
  expect(screen.getByText('解压分卷并合并')).toBeVisible();
  rerender(<ParseConfirmDialog result={{ ...result, taskId: 'empty', processing: [], discoveredPassword: undefined }} {...props} />);
  expect(screen.queryByRole('button', { name: /后处理与密码/ })).not.toBeInTheDocument();
  expect(screen.getByRole('dialog').querySelector('.processing-summary')).toBeNull();
});

it('uses the torrent root as the title, hides the local path, formats byte sizes, and lets the user choose this task destination', async () => {
  const user = userEvent.setup();
  const onConfirm = vi.fn();
  const browseDirectory = vi.fn().mockResolvedValue('E:\\Media');
  render(<ParseConfirmDialog
    result={{ ...result, title: 'magnet:?xt=urn:btih:fixture', resources: [
      { id: 'a', name: 'D://合集/episode.mp4', size: '0 B / 168996012 B', selected: true },
      { id: 'b', name: 'D://合集/readme.txt', size: '136 B', selected: true }
    ], isBitTorrent: true }}
    downloadDirectory={'D:\\'}
    browseDirectory={browseDirectory}
    onConfirm={onConfirm}
    onSelectionChange={() => {}}
    onClose={() => {}}
  />);

  expect(screen.getByText('合集')).toBeVisible();
  expect(screen.getByText('episode.mp4')).toBeVisible();
  expect(screen.queryByText('D://合集/episode.mp4')).not.toBeInTheDocument();
  expect(screen.getByText('161.2 MB')).toBeVisible();

  await user.click(screen.getByRole('button', { name: '选择下载目录' }));
  expect(browseDirectory).toHaveBeenCalledWith('D:\\');
  expect(await screen.findByText('E:\\Media')).toBeVisible();
  await user.click(screen.getByRole('button', { name: /立即下载 2/ }));
  expect(onConfirm).toHaveBeenCalledWith(['a', 'b'], 'E:\\Media');
});
