import { act, fireEvent, render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { vi } from 'vitest';
import { readFileSync } from 'node:fs';
import { LocalUploadPicker } from './LocalUploadPicker';
import type { LocalUploadListing } from '../lib/localUpload';

const home: LocalUploadListing = {
  path: '/home', parentPath: '/', roots: [{ path: '/', name: '根目录', kind: 'directory' }],
  entries: [{ path: '/home/Photos', name: 'Photos', kind: 'directory' }, { path: '/home/Docs', name: 'Docs', kind: 'directory' }, { path: '/home/a.txt', name: 'a.txt', kind: 'file' }, { path: '/home/b.txt', name: 'b.txt', kind: 'file' }]
};
const photos: LocalUploadListing = { path: '/home/Photos', parentPath: '/home', roots: home.roots, entries: [{ path: '/home/Photos/p.jpg', name: 'p.jpg', kind: 'file' }] };
const listing = () => vi.fn(async (path?: string) => path === '/home/Photos' ? photos : home);

it('hides dot files and directories until explicitly shown, including select-all', async () => {
  const user = userEvent.setup();
  const onConfirm = vi.fn();
  render(<LocalUploadPicker listEntries={async () => ({ ...home, entries: [...home.entries,
    { path: '/home/.config', name: '.config', kind: 'directory' },
    { path: '/home/.env', name: '.env', kind: 'file' },
  ] })} onConfirm={onConfirm} onCancel={vi.fn()} />);
  await screen.findByRole('checkbox', { name: '选择 a.txt' });
  expect(screen.queryByText('.config')).not.toBeInTheDocument();
  expect(screen.queryByText('.env')).not.toBeInTheDocument();
  await user.click(screen.getByRole('checkbox', { name: '全选当前目录' }));
  await user.click(screen.getByRole('button', { name: '确认上传' }));
  expect(onConfirm.mock.calls[0][0]).not.toContain('/home/.env');
  expect(onConfirm.mock.calls[0][0]).not.toContain('/home/.config');
  await user.click(screen.getByRole('checkbox', { name: '显示隐藏文件' }));
  expect(screen.getByRole('checkbox', { name: '选择 .config' })).toBeVisible();
  expect(screen.getByRole('checkbox', { name: '选择 .env' })).toBeVisible();
});

function visibleEntryNames(): string[] {
  return Array.from(document.querySelectorAll('.local-upload-entry:not(.local-upload-parent) .local-upload-entry-name span'))
    .map(element => element.textContent ?? '');
}

it('sorts upload entries by natural name in both directions while keeping directories first', async () => {
  const user = userEvent.setup();
  const entries = [
    { path: '/home/file10.zip', name: 'file10.zip', kind: 'file' as const, size: 10, modifiedAt: 200 },
    { path: '/home/Zeta', name: 'Zeta', kind: 'directory' as const, modifiedAt: 400 },
    { path: '/home/file2.zip', name: 'file2.zip', kind: 'file' as const, size: 20, modifiedAt: 300 },
    { path: '/home/Alpha', name: 'Alpha', kind: 'directory' as const, modifiedAt: 100 },
  ];
  render(<LocalUploadPicker listEntries={async () => ({ ...home, parentPath: '/', entries })} onConfirm={vi.fn()} onCancel={vi.fn()} />);
  await screen.findByRole('checkbox', { name: '选择 file10.zip' });

  const nameSort = screen.getByRole('button', { name: '按名称排序' });
  await user.click(nameSort);
  expect(nameSort.closest('[role="columnheader"]')).toHaveAttribute('aria-sort', 'ascending');
  expect(visibleEntryNames()).toEqual(['Alpha', 'Zeta', 'file2.zip', 'file10.zip']);
  expect(screen.getByRole('button', { name: '..' }).closest('.local-upload-file-list')?.firstElementChild)
    .toContainElement(screen.getByRole('button', { name: '..' }));

  await user.click(nameSort);
  expect(nameSort.closest('[role="columnheader"]')).toHaveAttribute('aria-sort', 'descending');
  expect(visibleEntryNames()).toEqual(['Zeta', 'Alpha', 'file10.zip', 'file2.zip']);
});

it('sorts upload files by byte size and modification timestamp without losing selection', async () => {
  const user = userEvent.setup();
  const entries = [
    { path: '/home/large.zip', name: 'large.zip', kind: 'file' as const, size: 9_000, modifiedAt: 100 },
    { path: '/home/unknown.zip', name: 'unknown.zip', kind: 'file' as const, size: null, modifiedAt: null },
    { path: '/home/small.zip', name: 'small.zip', kind: 'file' as const, size: 10, modifiedAt: 300 },
    { path: '/home/medium.zip', name: 'medium.zip', kind: 'file' as const, size: 500, modifiedAt: 200 },
  ];
  render(<LocalUploadPicker listEntries={async () => ({ ...home, entries })} onConfirm={vi.fn()} onCancel={vi.fn()} />);
  await user.click(await screen.findByRole('checkbox', { name: '选择 medium.zip' }));

  const sizeSort = screen.getByRole('button', { name: '按大小排序' });
  await user.click(sizeSort);
  expect(visibleEntryNames()).toEqual(['small.zip', 'medium.zip', 'large.zip', 'unknown.zip']);
  await user.click(sizeSort);
  expect(visibleEntryNames()).toEqual(['large.zip', 'medium.zip', 'small.zip', 'unknown.zip']);

  const modifiedSort = screen.getByRole('button', { name: '按时间排序' });
  await user.click(modifiedSort);
  expect(visibleEntryNames()).toEqual(['large.zip', 'medium.zip', 'small.zip', 'unknown.zip']);
  await user.click(modifiedSort);
  expect(visibleEntryNames()).toEqual(['small.zip', 'medium.zip', 'large.zip', 'unknown.zip']);
  expect(screen.getByRole('checkbox', { name: '选择 medium.zip' })).toBeChecked();
});

it('keeps upload names and metadata readable in compact rows', async () => {
  const style = document.createElement('style');
  style.textContent = readFileSync('src/styles.css', 'utf8');
  document.head.appendChild(style);
  try {
    render(<LocalUploadPicker listEntries={listing()} onConfirm={vi.fn()} onCancel={vi.fn()} />);
    const name = await screen.findByRole('button', { name: 'a.txt' });
    const row = name.closest('.local-upload-entry')!;
    expect(getComputedStyle(name).fontSize).toBe('14px');
    expect(getComputedStyle(row.querySelector('small')!).fontSize).toBe('14px');
    expect(getComputedStyle(row).minHeight).toBe('36px');
    expect(getComputedStyle(screen.getByRole('button', { name: '确认上传' })).fontSize).toBe('14px');
  } finally { style.remove(); }
});

it('confirms multiple files and multiple directories together without a type selector', async () => {
  const user = userEvent.setup();
  const onConfirm = vi.fn();
  render(<LocalUploadPicker listEntries={listing()} onConfirm={onConfirm} onCancel={vi.fn()} />);
  expect(screen.getByRole('button', { name: '确认上传' })).toBeDisabled();
  await user.click(await screen.findByRole('checkbox', { name: '选择 Photos' }));
  await user.click(screen.getByRole('checkbox', { name: '选择 Docs' }));
  await user.click(screen.getByRole('checkbox', { name: '选择 a.txt' }));
  await user.click(screen.getByRole('checkbox', { name: '选择 b.txt' }));
  await user.click(screen.getByRole('button', { name: '确认上传' }));
  expect(onConfirm).toHaveBeenCalledWith(['/home/Photos', '/home/Docs', '/home/a.txt', '/home/b.txt']);
});

it('keeps cross-directory selections and excludes selected descendants when their parent is selected', async () => {
  const user = userEvent.setup();
  const onConfirm = vi.fn();
  render(<LocalUploadPicker listEntries={listing()} onConfirm={onConfirm} onCancel={vi.fn()} />);
  await user.dblClick(await screen.findByRole('button', { name: 'Photos' }));
  await user.click(await screen.findByRole('checkbox', { name: '选择 p.jpg' }));
  await user.click(screen.getByRole('button', { name: '..' }));
  await user.click(await screen.findByRole('checkbox', { name: '选择 Photos' }));
  await user.click(screen.getByRole('checkbox', { name: '选择 a.txt' }));
  await user.click(screen.getByRole('button', { name: '确认上传' }));
  expect(onConfirm).toHaveBeenCalledWith(['/home/Photos', '/home/a.txt']);
});

it('selects all visible entries and lets users remove selections before confirming', async () => {
  const user = userEvent.setup();
  const onConfirm = vi.fn();
  render(<LocalUploadPicker listEntries={listing()} onConfirm={onConfirm} onCancel={vi.fn()} />);
  await user.click(await screen.findByRole('checkbox', { name: '全选当前目录' }));
  await user.click(screen.getByRole('checkbox', { name: '选择 a.txt' }));
  await user.click(screen.getByRole('button', { name: '确认上传' }));
  expect(onConfirm).toHaveBeenCalledWith(['/home/Photos', '/home/Docs', '/home/b.txt']);
});

it('cancels with Escape without submitting any files', async () => {
  const user = userEvent.setup();
  const onConfirm = vi.fn(), onCancel = vi.fn();
  render(<LocalUploadPicker listEntries={listing()} onConfirm={onConfirm} onCancel={onCancel} />);
  await user.click(await screen.findByRole('checkbox', { name: '选择 a.txt' }));
  await user.keyboard('{Escape}');
  expect(onCancel).toHaveBeenCalledTimes(1);
  expect(onConfirm).not.toHaveBeenCalled();
});

it('shows a path error without dropping previous selections and allows recovery', async () => {
  const user = userEvent.setup();
  const listEntries = vi.fn(async (path?: string) => { if (path === '/missing') throw new Error('目录不存在'); return home; });
  const onConfirm = vi.fn();
  render(<LocalUploadPicker listEntries={listEntries} onConfirm={onConfirm} onCancel={vi.fn()} />);
  await user.click(await screen.findByRole('checkbox', { name: '选择 a.txt' }));
  await user.dblClick(screen.getByRole('navigation', { name: '本地目录路径' }));
  const path = screen.getByRole('textbox', { name: '本地目录路径' });
  await user.clear(path); await user.type(path, '/missing{Enter}');
  expect(await screen.findByRole('alert')).toHaveTextContent('目录不存在');
  await user.clear(path); await user.type(path, '/home{Enter}');
  await waitFor(() => expect(screen.queryByRole('alert')).not.toBeInTheDocument());
  await user.click(screen.getByRole('button', { name: '确认上传' }));
  expect(onConfirm).toHaveBeenCalledWith(['/home/a.txt']);
});

it('ignores a late listing after navigating to a newer path', async () => {
  let finish!: (result: LocalUploadListing) => void;
  const listEntries = vi.fn().mockResolvedValueOnce(home).mockImplementationOnce(() => new Promise(resolve => { finish = resolve; })).mockResolvedValueOnce(photos);
  render(<LocalUploadPicker listEntries={listEntries} onConfirm={vi.fn()} onCancel={vi.fn()} />);
  await screen.findByRole('checkbox', { name: '选择 a.txt' });
  fireEvent.doubleClick(screen.getByRole('navigation', { name: '本地目录路径' }));
  const path = screen.getByRole('textbox', { name: '本地目录路径' });
  fireEvent.change(path, { target: { value: '/slow' } }); fireEvent.submit(path.closest('form')!);
  fireEvent.change(path, { target: { value: '/home/Photos' } }); fireEvent.submit(path.closest('form')!);
  await screen.findByRole('checkbox', { name: '选择 p.jpg' });
  await act(async () => { finish(home); });
  expect(screen.getByRole('checkbox', { name: '选择 p.jpg' })).toBeVisible();
  expect(screen.getByRole('navigation', { name: '本地目录路径' })).toHaveTextContent('Photos');
});

it('restores the partial-selection indicator after reloading the same directory', async () => {
  const user = userEvent.setup();
  const listEntries = listing();
  render(<LocalUploadPicker listEntries={listEntries} onConfirm={vi.fn()} onCancel={vi.fn()} />);
  await user.click(await screen.findByRole('checkbox', { name: '选择 a.txt' }));
  expect(screen.getByRole('checkbox', { name: '全选当前目录' })).toBePartiallyChecked();
  let finish!: (value: LocalUploadListing) => void;
  listEntries.mockImplementationOnce(() => new Promise(resolve => { finish = resolve; }));
  await user.dblClick(screen.getByRole('navigation', { name: '本地目录路径' }));
  await user.keyboard('{Enter}');
  expect(screen.queryByRole('checkbox', { name: '全选当前目录' })).not.toBeInTheDocument();
  await act(async () => { finish(home); });
  expect(screen.getByRole('checkbox', { name: '全选当前目录' })).toBePartiallyChecked();
});

it('returns via the first .. row without adding the parent to uploads', async () => {
  const user = userEvent.setup();
  const confirm = vi.fn();
  render(<LocalUploadPicker listEntries={listing()} onConfirm={confirm} onCancel={vi.fn()} />);
  await user.dblClick(await screen.findByRole('button', { name: 'Photos' }));
  await user.click(await screen.findByRole('checkbox', { name: '选择 p.jpg' }));
  const parent = screen.getByRole('button', { name: '..' });
  expect(parent.closest('.local-upload-file-list')?.firstElementChild).toContainElement(parent);
  await user.click(parent);
  await screen.findByRole('checkbox', { name: '选择 a.txt' });
  await user.click(screen.getByRole('button', { name: '确认上传' }));
  expect(confirm).toHaveBeenCalledWith(['/home/Photos/p.jpg']);
});

it('navigates a breadcrumb on single click but double click edits the original full path', async () => {
  const user = userEvent.setup();
  const listEntries = listing();
  render(<LocalUploadPicker listEntries={listEntries} onConfirm={vi.fn()} onCancel={vi.fn()} />);
  await user.dblClick(await screen.findByRole('button', { name: 'Photos' }));
  await screen.findByRole('checkbox', { name: '选择 p.jpg' });
  await user.dblClick(screen.getByRole('button', { name: '跳转到 /home' }));
  const input = screen.getByRole('textbox', { name: '本地目录路径' }) as HTMLInputElement;
  expect(input).toHaveValue('/home/Photos');
  expect(input).toHaveFocus();
  expect(input.selectionStart).toBe(0);
  expect(input.selectionEnd).toBe(12);
  await user.keyboard('{Escape}');
  await user.click(screen.getByRole('button', { name: '跳转到 /home' }));
  await screen.findByRole('checkbox', { name: '选择 a.txt' });
  expect(listEntries).toHaveBeenCalledTimes(3);
});

it('renders size and modification time without instruction text or a selection side panel', async () => {
  const user = userEvent.setup();
  const entries = [
    { path: '/home/folder', name: 'folder', kind: 'directory' as const, size: null, modifiedAt: new Date(2026, 8, 13, 10, 20, 30).getTime() },
    { path: '/home/a.txt', name: 'a.txt', kind: 'file' as const, size: 1536, modifiedAt: new Date(2026, 8, 13, 11, 22, 33).getTime() },
    { path: '/home/empty', name: 'empty', kind: 'file' as const, size: 0, modifiedAt: null }
  ];
  render(<LocalUploadPicker listEntries={async () => ({ ...home, entries })} onConfirm={vi.fn()} onCancel={vi.fn()} />);
  await user.click(await screen.findByRole('checkbox', { name: '选择 a.txt' }));
  expect(screen.getByText('1.5 KB')).toBeVisible();
  expect(screen.getByText('0 B')).toBeVisible();
  expect(screen.getByText('2026-09-13 10:20:30')).toBeVisible();
  expect(screen.getByText('2026-09-13 11:22:33')).toBeVisible();
  expect(screen.queryByRole('region', { name: '已选上传项目' })).not.toBeInTheDocument();
  expect(screen.queryByText(/双击目录进入浏览|目录将保留原有层级/)).not.toBeInTheDocument();
  expect(screen.queryByRole('button', { name: '前往' })).not.toBeInTheDocument();
  expect(screen.queryByRole('button', { name: '上一级' })).not.toBeInTheDocument();
});

it('cancels a pending breadcrumb click when a newer navigation starts', async () => {
  let finish!: (value: LocalUploadListing) => void;
  const listEntries = vi.fn().mockResolvedValueOnce(home)
    .mockImplementationOnce(() => new Promise(resolve => { finish = resolve; }))
    .mockResolvedValue(home);
  render(<LocalUploadPicker listEntries={listEntries} onConfirm={vi.fn()} onCancel={vi.fn()} />);
  await screen.findByRole('checkbox', { name: '选择 a.txt' });
  vi.useFakeTimers();
  try {
    fireEvent.click(screen.getByRole('button', { name: '跳转到 /home' }), { detail: 1 });
    fireEvent.click(screen.getByRole('button', { name: '用户目录' }));
    await act(async () => { await vi.advanceTimersByTimeAsync(600); });
    await act(async () => { finish(photos); });
    expect(screen.getByRole('checkbox', { name: '选择 p.jpg' })).toBeVisible();
  } finally { vi.useRealTimers(); }
});

it('respects the system double-click interval when editing a breadcrumb', async () => {
  const listEntries = vi.fn(async () => ({ ...photos, doubleClickTimeMs: 900 }));
  render(<LocalUploadPicker listEntries={listEntries} onConfirm={vi.fn()} onCancel={vi.fn()} />);
  await screen.findByRole('checkbox', { name: '选择 p.jpg' });
  vi.useFakeTimers();
  try {
    const parent = screen.getByRole('button', { name: '跳转到 /home' });
    fireEvent.click(parent, { detail: 1 });
    await act(async () => { await vi.advanceTimersByTimeAsync(650); });
    fireEvent.click(parent, { detail: 2 });
    fireEvent.doubleClick(parent);
    await act(async () => { await vi.advanceTimersByTimeAsync(1000); });
    expect(screen.getByRole('textbox', { name: '本地目录路径' })).toHaveValue('/home/Photos');
    expect(listEntries).toHaveBeenCalledTimes(1);
  } finally { vi.useRealTimers(); }
});
