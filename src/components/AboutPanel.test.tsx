import { render, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { vi } from 'vitest';
import { AboutPanel } from './AboutPanel';

it('shows project acknowledgements and the ten declaration terms after usage instructions', () => {
  render(<AboutPanel icon="/icon.png" />);
  const help = screen.getByRole('region', { name: '使用说明' });
  const thanks = screen.getByRole('region', { name: '致谢' });
  const declaration = screen.getByRole('region', { name: '声明' });
  expect(help.compareDocumentPosition(thanks) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  expect(thanks.compareDocumentPosition(declaration) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  expect(within(thanks).getByRole('link', { name: 'aria2' })).toHaveAttribute('href', 'https://aria2.github.io/');
  expect(within(thanks).getByRole('link', { name: 'aria2-next' })).toHaveAttribute('href', 'https://github.com/AnInsomniacy/aria2-next');
  expect(within(thanks).getByRole('link', { name: 'Lucide' })).toHaveAttribute('href', 'https://lucide.dev/');
  expect(within(declaration).getAllByRole('listitem')).toHaveLength(10);
  expect(within(declaration).getByText('下载或使用本项目，即视为您已阅读并接受上述全部条款。')).toBeVisible();
  expect(screen.queryByRole('complementary', { name: '使用提醒' })).not.toBeInTheDocument();
});

it('disables manual checking while checking and distinguishes failure from no updates', async () => {
  const onCheckUpdate = vi.fn();
  const { rerender } = render(<AboutPanel icon="/icon.png" checkStatus="checking" onCheckUpdate={onCheckUpdate} />);
  const check = screen.getByRole('button', { name: '检查更新' });
  expect(check).toBeDisabled();
  expect(screen.getByRole('status')).toHaveTextContent('正在检查更新…');
  await userEvent.click(check);
  expect(onCheckUpdate).not.toHaveBeenCalled();

  rerender(<AboutPanel icon="/icon.png" checkStatus="error" onCheckUpdate={onCheckUpdate} />);
  expect(check).toBeEnabled();
  expect(screen.getByRole('status')).toHaveTextContent('暂时无法检查更新，请稍后重试。');
  expect(screen.queryByText('当前没有可用更新。')).not.toBeInTheDocument();
  await userEvent.click(check);
  expect(onCheckUpdate).toHaveBeenCalledOnce();

  rerender(<AboutPanel icon="/icon.png" checkStatus="upToDate" onCheckUpdate={onCheckUpdate} />);
  expect(screen.getByRole('status')).toHaveTextContent('当前没有可用更新。');
});

it('shows release notes as text and a localized fallback when they are empty', () => {
  const update = { version: '0.3.1', notes: '<b>Release notes</b>', downloadUrl: 'https://www.versadown.com/#gate' };
  const { rerender } = render(<AboutPanel icon="/icon.png" update={update} checkStatus="available" />);
  const notes = screen.getByText('<b>Release notes</b>');
  expect(notes.querySelector('b')).toBeNull();
  expect(screen.getByRole('button', { name: '去官网下载' })).toBeVisible();

  rerender(<AboutPanel icon="/icon.png" update={{ ...update, notes: '' }} checkStatus="available" />);
  expect(screen.getByText('前往官网查看本次更新内容。')).toBeVisible();
});
