import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { vi } from 'vitest';
import { AgentCloudGrants } from './AgentCloudGrants';

it('explains on-demand account access without a folder editor', () => {
  const change = vi.fn();
  render(<AgentCloudGrants grants={[]} onChange={change} />);
  expect(screen.getByText(/尚未授权网盘账号/)).toBeVisible();
  expect(screen.queryByRole('button')).not.toBeInTheDocument();
  expect(screen.queryByRole('combobox')).not.toBeInTheDocument();
  expect(change).not.toHaveBeenCalled();
});

it('removes a saved account without exposing its private identifier', async () => {
  const user = userEvent.setup();
  const first = { provider: 'quark' as const, accountId: 'private-id', accountLabel: 'My saved account' };
  const second = { provider: 'pikpak' as const, accountId: 'other-id', accountLabel: 'Other account' };
  const change = vi.fn();
  render(<AgentCloudGrants grants={[first, second]} onChange={change} />);
  expect(screen.getByText('夸克网盘 · My saved account')).toBeVisible();
  expect(screen.queryByText('private-id')).not.toBeInTheDocument();
  await user.click(screen.getByRole('button', { name: '撤销第 1 项网盘账号授权' }));
  expect(change).toHaveBeenCalledWith([second]);
});

it('blocks repeated changes while permissions are being saved', () => {
  const change = vi.fn();
  render(<AgentCloudGrants grants={[{ provider: 'quark', accountId: 'private-id' }]} onChange={change} disabled />);
  expect(screen.getByRole('button', { name: '撤销第 1 项网盘账号授权' })).toBeDisabled();
  expect(change).not.toHaveBeenCalled();
});
