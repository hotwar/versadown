import { useEffect, useRef, useState } from 'react';
import { useTranslation } from '../i18n';
import * as cloudApi from '../lib/cloudApi';
import type { CloudAccount, CloudDirectory, ProviderDescriptor } from '../lib/cloudTypes';
import type { AgentCloudGrant, AgentCloudOperation } from '../lib/types';

const operations: AgentCloudOperation[] = ['browse', 'download', 'upload', 'create_share'];
const capability = { browse: 'list', download: 'download', upload: 'upload', create_share: 'shareCreate', transfer: 'shareTransfer' } as const;
type Client = Pick<typeof cloudApi, 'getCloudProviderCatalog' | 'getCloudAccount' | 'listCloudDirectory'>;
type Breadcrumb = { id: string; name: string };

interface Props {
  grants: AgentCloudGrant[];
  onChange(grants: AgentCloudGrant[]): void;
  disabled?: boolean;
  client?: Client;
}

export function AgentCloudGrants({ grants, onChange, disabled, client = cloudApi }: Props) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const [catalog, setCatalog] = useState<ProviderDescriptor[]>([]);
  const [accounts, setAccounts] = useState<CloudAccount[]>([]);
  const [accountKey, setAccountKey] = useState('');
  const [directory, setDirectory] = useState<CloudDirectory | null>(null);
  const [breadcrumbs, setBreadcrumbs] = useState<Breadcrumb[]>([]);
  const [selectedOperations, setSelectedOperations] = useState<AgentCloudOperation[]>(['browse', 'download']);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const generation = useRef(0);
  useEffect(() => () => { generation.current += 1; }, []);
  const keyFor = (account: CloudAccount) => `${account.provider}:${account.id}`;
  const selectedAccount = accounts.find(account => keyFor(account) === accountKey);
  const selectedProvider = catalog.find(provider => provider.provider === selectedAccount?.provider);

  async function loadAccounts() {
    const request = ++generation.current;
    setOpen(true); setLoading(true); setError(null);
    setAccountKey(''); setDirectory(null); setBreadcrumbs([]); setAccounts([]);
    try {
      const descriptors = await client.getCloudProviderCatalog();
      const results = await Promise.allSettled(descriptors.map(({ provider }) => client.getCloudAccount(provider)));
      if (generation.current !== request) return;
      setCatalog(descriptors);
      setAccounts(results.flatMap((result, index) => result.status === 'fulfilled' && result.value.provider === descriptors[index].provider ? [result.value] : []));
    } catch {
      if (generation.current === request) setError(t('settings.agent.error.cloudAccounts'));
    } finally {
      if (generation.current === request) setLoading(false);
    }
  }

  async function loadDirectory(account: CloudAccount, parentId: string, trail: Breadcrumb[]) {
    const request = ++generation.current;
    setLoading(true); setError(null); setDirectory(null);
    try {
      const listing = await client.listCloudDirectory(account.provider, parentId);
      if (generation.current !== request) return;
      setDirectory(listing);
      setBreadcrumbs(trail.map((item, index) => index === trail.length - 1 ? { ...item, id: listing.id } : item));
    } catch {
      if (generation.current === request) setError(t('settings.agent.error.cloudVerification'));
    } finally {
      if (generation.current === request) setLoading(false);
    }
  }

  function selectAccount(key: string) {
    setAccountKey(key); setDirectory(null); setBreadcrumbs([]);
    const account = accounts.find(item => keyFor(item) === key);
    const provider = catalog.find(item => item.provider === account?.provider);
    if (!account || !provider) { generation.current += 1; setLoading(false); return; }
    setSelectedOperations(['browse', 'download'].filter(operation => provider.capabilities[capability[operation as AgentCloudOperation]]) as AgentCloudOperation[]);
    void loadDirectory(account, provider.rootDirectoryId, [{ id: provider.rootDirectoryId, name: t('cloud.root') }]);
  }

  function addGrant() {
    if (!selectedAccount || !directory || !selectedOperations.length || loading) return;
    const grant: AgentCloudGrant = { provider: selectedAccount.provider, accountId: selectedAccount.id, directoryIds: [directory.id], operations: [...selectedOperations],
      accountLabel: selectedAccount.displayName, directoryLabels: { [directory.id]: breadcrumbs.map(item => item.name).join(' / ') } };
    onChange([...grants, grant]);
    setOpen(false); setDirectory(null); generation.current += 1;
  }

  return <section className="agent-cloud-grants" aria-label={t('settings.agent.cloudGrants')}>
    <b>{t('settings.agent.cloudGrants')}</b>
    <p className="agent-access-description">{t('settings.agent.cloudHint')}</p>
    {grants.length === 0 && <p className="agent-access-description">{t('settings.agent.noCloudGrants')}</p>}
    {grants.map((grant, index) => {
      const account = accounts.find(item => item.provider === grant.provider && item.id === grant.accountId);
      return <div className="setting-row" key={index}>
        <span><b>{t(`cloud.provider.${grant.provider}`)} · {account?.displayName ?? grant.accountLabel ?? t('settings.agent.savedAccount')}</b>
          <small>{grant.directoryIds.map(id => grant.directoryLabels?.[id] ?? t('settings.agent.cloudDirectory')).join(' / ')} · {grant.operations.map(operation => t(`settings.agent.operation.${operation}`)).join(' / ')}</small></span>
        <button type="button" disabled={disabled} onClick={() => onChange(grants.filter((_, position) => position !== index))} aria-label={t('settings.agent.removeCloudGrant', { number: index + 1 })}>{t('settings.agent.remove')}</button>
      </div>;
    })}
    {!open && <button type="button" disabled={disabled || grants.length >= 32} onClick={() => void loadAccounts()}>{t('settings.agent.addCloudGrant')}</button>}
    {open && <fieldset disabled={disabled} className="agent-cloud-picker">
      <label><span>{t('settings.agent.cloudAccount')}</span><select aria-label={t('settings.agent.cloudAccount')} value={accountKey} disabled={loading} onChange={event => selectAccount(event.target.value)}>
        <option value="">{t('settings.agent.chooseAccount')}</option>
        {accounts.map(account => <option key={keyFor(account)} value={keyFor(account)}>{t(`cloud.provider.${account.provider}`)} · {account.displayName}</option>)}
      </select></label>
      {!loading && !accounts.length && <p>{t('settings.agent.noAccounts')}</p>}
      {selectedAccount && <>
        <nav aria-label={t('settings.agent.cloudDirectory')}>
          {breadcrumbs.map((item, index) => <button key={`${index}:${item.id}`} type="button" disabled={loading || index === breadcrumbs.length - 1} onClick={() => void loadDirectory(selectedAccount, item.id, breadcrumbs.slice(0, index + 1))}>{item.name}</button>)}
        </nav>
        {directory && <ul>{directory.items.filter(item => item.kind === 'folder' && item.id !== selectedProvider?.recycleBinDirectoryId).map(folder => <li key={folder.id}><button type="button" onClick={() => void loadDirectory(selectedAccount, folder.id, [...breadcrumbs, { id: folder.id, name: folder.name }])}>{folder.name}</button></li>)}</ul>}
        <div className="agent-cloud-operations">{operations.filter(operation => selectedProvider?.capabilities[capability[operation]]).map(operation => <label key={operation}><input type="checkbox" checked={selectedOperations.includes(operation)} onChange={event => setSelectedOperations(current => event.target.checked ? [...current, operation] : current.filter(item => item !== operation))} />{t(`settings.agent.operation.${operation}`)}</label>)}</div>
        <p className="agent-access-description">{t('settings.agent.cloudDescendants')}</p>
      </>}
      {loading && <p role="status">{t('cloud.loadingDirectory')}</p>}
      {error && <p role="alert">{error}</p>}
      <div className="agent-access-actions">
        <button type="button" disabled={loading} onClick={() => void loadAccounts()}>{t('settings.agent.refreshAccounts')}</button>
        <button type="button" onClick={() => { generation.current += 1; setOpen(false); setLoading(false); }}>{t('dialog.cancel')}</button>
        <button type="button" disabled={loading || !directory || !selectedOperations.length} onClick={addGrant}>{t('settings.agent.grantCurrentDirectory')}</button>
      </div>
    </fieldset>}
  </section>;
}
