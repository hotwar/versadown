import { useEffect, useMemo, useRef, useState, type Dispatch, type PointerEvent as ReactPointerEvent, type SetStateAction } from 'react';
import type { CloudAccount, CloudProvider, ProviderDescriptor } from '../lib/cloudTypes';
import { accountRecoveryAction, CloudManager, CloudManagerClient, errorMessage } from './CloudManager';
import { useTranslation } from '../i18n';
import { cloudProviderLabel } from '../i18n/cloudProviderLabel';

export interface CloudWorkspaceProps {
  client: CloudManagerClient;
  activePage: CloudWorkspacePage;
  onPageChange: Dispatch<SetStateAction<CloudWorkspacePage>>;
  onProvidersChange(providers: readonly ProviderDescriptor[]): void;
  onDownloadTask?(task: unknown): void;
  privacyDownloadEnabled?: boolean;
}

export type CloudWorkspacePage = 'management' | CloudProvider;

const cloudProviderCardOrderStorageKey = 'versadown.cloud-provider-card-order';
type PendingProviderCardDrag = {
  provider: CloudProvider;
  pointerId: number;
  source: HTMLElement;
  originX: number;
  originY: number;
  offsetX: number;
  offsetY: number;
  order: readonly CloudProvider[];
  previewOrder: readonly CloudProvider[] | null;
  preview: HTMLElement | null;
};

function savedProviderCardOrder(): CloudProvider[] {
  try {
    const saved = JSON.parse(window.localStorage.getItem(cloudProviderCardOrderStorageKey) ?? '[]');
    return Array.isArray(saved) ? saved.filter((provider): provider is CloudProvider => typeof provider === 'string') : [];
  } catch {
    return [];
  }
}

function orderedProviders(catalog: readonly ProviderDescriptor[], preferredOrder: readonly CloudProvider[]): readonly ProviderDescriptor[] {
  const catalogByProvider = new Map(catalog.map((descriptor) => [descriptor.provider, descriptor]));
  const knownOrder = preferredOrder.filter((provider, index) => catalogByProvider.has(provider) && preferredOrder.indexOf(provider) === index);
  return [...knownOrder.map((provider) => catalogByProvider.get(provider)!), ...catalog.filter((descriptor) => !knownOrder.includes(descriptor.provider))];
}

function reorderProviderCards(order: readonly CloudProvider[], provider: CloudProvider, target: CloudProvider, placeAfter: boolean): CloudProvider[] {
  if (provider === target) return [...order];
  const withoutProvider = order.filter((item) => item !== provider);
  const targetIndex = withoutProvider.indexOf(target);
  if (targetIndex < 0) return [...order];
  const insertAt = targetIndex + (placeAfter ? 1 : 0);
  return [...withoutProvider.slice(0, insertAt), provider, ...withoutProvider.slice(insertAt)];
}

function saveProviderCardOrder(order: readonly CloudProvider[]) {
  try {
    window.localStorage.setItem(cloudProviderCardOrderStorageKey, JSON.stringify(order));
  } catch {
    // Sorting is still available when persistent storage is unavailable.
  }
}

type AccountRecoveryError = { action: 'retry' | 'expired' | 'web'; message: string };

export function CloudWorkspace({ client, activePage, onPageChange, onProvidersChange, onDownloadTask, privacyDownloadEnabled = false }: CloudWorkspaceProps) {
  const { t } = useTranslation();
  const [accounts, setAccounts] = useState<readonly CloudAccount[]>([]);
  const [catalog, setCatalog] = useState<readonly ProviderDescriptor[]>([]);
  const [pendingProviders, setPendingProviders] = useState<readonly CloudProvider[]>([]);
  const [loadingCatalog, setLoadingCatalog] = useState(true);
  const recoveringAccounts = loadingCatalog || pendingProviders.length > 0;
  const [recoveryError, setRecoveryError] = useState<string | null>(null);
  const [accountErrors, setAccountErrors] = useState<Partial<Record<CloudProvider, AccountRecoveryError>>>({});
  const recoveryGeneration = useRef(0);
  const [visitedAccounts, setVisitedAccounts] = useState<ReadonlySet<string>>(() => new Set());
  const [providerCardOrder, setProviderCardOrder] = useState<readonly CloudProvider[]>(savedProviderCardOrder);
  const [providerCardPreviewOrder, setProviderCardPreviewOrder] = useState<readonly CloudProvider[] | null>(null);
  const [draggingProvider, setDraggingProvider] = useState<CloudProvider | null>(null);
  const [dropTargetProvider, setDropTargetProvider] = useState<CloudProvider | null>(null);
  const pendingProviderCardDrag = useRef<PendingProviderCardDrag | null>(null);
  const activeAccount = activePage === 'management' ? null : accounts.find((account) => account.provider === activePage) ?? null;
  const accountKey = (account: CloudAccount) => `${account.provider}:${account.id}`;
  const activeKey = activeAccount ? accountKey(activeAccount) : null;
  const authorizedProviders = useMemo(() => catalog.filter(descriptor => accounts.some(account => account.provider === descriptor.provider)), [catalog, accounts]);
  const managementCatalog = useMemo(() => orderedProviders(catalog, providerCardPreviewOrder ?? providerCardOrder), [catalog, providerCardOrder, providerCardPreviewOrder]);

  useEffect(() => { onProvidersChange(authorizedProviders); }, [authorizedProviders, onProvidersChange]);

  useEffect(() => {
    setVisitedAccounts(current => {
      const next = new Set(accounts.map(accountKey).filter(key => current.has(key) || key === activeKey));
      return next.size === current.size && [...next].every(key => current.has(key)) ? current : next;
    });
  }, [accounts, activeKey]);

  useEffect(() => () => { pendingProviderCardDrag.current?.preview?.remove(); }, []);

  function acceptAccount(account: CloudAccount) {
    setAccountErrors(current => {
      const next = { ...current };
      delete next[account.provider];
      return next;
    });
    setAccounts((current) => current.some((item) => item.provider === account.provider)
      ? current.map((item) => item.provider === account.provider ? account : item)
      : [...current, account]);
  }

  async function restoreProvider(provider: CloudProvider, generation = recoveryGeneration.current) {
    setPendingProviders(current => [...current, provider]);
    try {
      const account = await client.getCloudAccount(provider);
      if (generation !== recoveryGeneration.current) return;
      if (account.provider !== provider) throw new Error(t('cloud.authorization.providerMismatch'));
      acceptAccount(account);
    } catch (error) {
      if (generation !== recoveryGeneration.current) return;
      const action = accountRecoveryAction(error);
      setAccountErrors(current => {
        const next = { ...current };
        if (action === 'authorize') delete next[provider];
        else next[provider] = { action, message: errorMessage(error, t('cloud.authorization.restoreSessionFailed')) };
        return next;
      });
    } finally {
      if (generation === recoveryGeneration.current) setPendingProviders(current => current.filter(item => item !== provider));
    }
  }

  useEffect(() => {
    const generation = ++recoveryGeneration.current;
    setLoadingCatalog(true);
    setRecoveryError(null);
    setAccountErrors({});
    setAccounts([]);
    setCatalog([]);
    setPendingProviders([]);
    onPageChange('management');
    void client.getCloudProviderCatalog().then((descriptors) => {
      if (generation !== recoveryGeneration.current) return;
      setCatalog(descriptors);
      setLoadingCatalog(false);
      for (const { provider } of descriptors) void restoreProvider(provider, generation);
    }).catch(error => {
      if (generation === recoveryGeneration.current) {
        setLoadingCatalog(false);
        setRecoveryError(errorMessage(error, t('cloud.error.readDirectory')));
      }
    });
    return () => { recoveryGeneration.current += 1; };
  }, [client, onPageChange]);

  function removeAccount(provider: CloudProvider) {
    setAccounts((current) => current.filter((account) => account.provider !== provider));
    onPageChange(current => current === provider ? 'management' : current);
  }

  function beginProviderCardDrag(provider: CloudProvider, event: ReactPointerEvent<HTMLElement>) {
    if (event.button !== 0 || (event.target as Element).closest('button, input, textarea, select, a')) return;
    const card = event.currentTarget.closest<HTMLElement>('.cloud-manager');
    if (!card) return;
    const bounds = card.getBoundingClientRect();
    pendingProviderCardDrag.current = {
      provider,
      pointerId: event.pointerId,
      source: event.currentTarget,
      originX: event.clientX,
      originY: event.clientY,
      offsetX: event.clientX - bounds.left,
      offsetY: event.clientY - bounds.top,
      order: orderedProviders(catalog, providerCardOrder).map((descriptor) => descriptor.provider),
      previewOrder: null,
      preview: null,
    };
    event.currentTarget.setPointerCapture?.(event.pointerId);
  }

  function startProviderCardPreview(pending: PendingProviderCardDrag) {
    if (pending.preview) return;
    const card = pending.source.closest<HTMLElement>('.cloud-manager');
    if (!card) return;
    const bounds = card.getBoundingClientRect();
    const preview = card.cloneNode(true) as HTMLElement;
    preview.classList.add('cloud-provider-drag-preview');
    preview.setAttribute('aria-hidden', 'true');
    preview.style.width = `${bounds.width}px`;
    preview.style.height = `${bounds.height}px`;
    document.body.append(preview);
    pending.preview = preview;
    setDraggingProvider(pending.provider);
  }

  function providerCardAtPointer(event: ReactPointerEvent<HTMLElement>) {
    const directCard = document.elementFromPoint(event.clientX, event.clientY)?.closest<HTMLElement>('[data-cloud-provider]');
    if (directCard?.dataset.cloudProvider) return directCard;
    const grid = event.currentTarget as HTMLElement;
    const bounds = grid.getBoundingClientRect();
    if (event.clientX < bounds.left || event.clientX > bounds.right || event.clientY < bounds.top || event.clientY > bounds.bottom) return null;
    return [...grid.querySelectorAll<HTMLElement>('[data-cloud-provider]')].sort((left, right) => {
      const leftBounds = left.getBoundingClientRect();
      const rightBounds = right.getBoundingClientRect();
      const leftDistance = Math.hypot(event.clientX - (leftBounds.left + leftBounds.width / 2), event.clientY - (leftBounds.top + leftBounds.height / 2));
      const rightDistance = Math.hypot(event.clientX - (rightBounds.left + rightBounds.width / 2), event.clientY - (rightBounds.top + rightBounds.height / 2));
      return leftDistance - rightDistance;
    })[0] ?? null;
  }

  function updateProviderCardDrag(event: ReactPointerEvent<HTMLElement>) {
    const pending = pendingProviderCardDrag.current;
    if (!pending || pending.pointerId !== event.pointerId) return;
    if (!pending.preview && Math.hypot(event.clientX - pending.originX, event.clientY - pending.originY) < 5) return;
    startProviderCardPreview(pending);
    if (!pending.preview) return;
    event.preventDefault();
    pending.preview.style.left = `${event.clientX - pending.offsetX}px`;
    pending.preview.style.top = `${event.clientY - pending.offsetY}px`;
    const targetCard = providerCardAtPointer(event);
    const target = targetCard?.dataset.cloudProvider as CloudProvider | undefined;
    if (!target || target === pending.provider) return;
    const bounds = targetCard!.getBoundingClientRect();
    const placeAfter = event.clientY > bounds.top + bounds.height / 2
      || (event.clientY >= bounds.top + bounds.height / 4 && event.clientY <= bounds.top + bounds.height * 3 / 4 && event.clientX >= bounds.left + bounds.width / 2);
    const nextOrder = reorderProviderCards(pending.order, pending.provider, target, placeAfter);
    if (nextOrder.every((item, index) => item === pending.previewOrder?.[index])) return;
    pending.previewOrder = nextOrder;
    setProviderCardPreviewOrder(nextOrder);
    setDropTargetProvider(target);
  }

  function clearProviderCardDrag(pending: PendingProviderCardDrag) {
    pending.source.releasePointerCapture?.(pending.pointerId);
    pending.preview?.remove();
    pendingProviderCardDrag.current = null;
    setProviderCardPreviewOrder(null);
    setDraggingProvider(null);
    setDropTargetProvider(null);
  }

  function finishProviderCardDrag(event: ReactPointerEvent<HTMLElement>) {
    const pending = pendingProviderCardDrag.current;
    if (!pending || pending.pointerId !== event.pointerId) return;
    updateProviderCardDrag(event);
    const nextOrder = pending.previewOrder;
    clearProviderCardDrag(pending);
    if (!nextOrder) return;
    setProviderCardOrder(nextOrder);
    saveProviderCardOrder(nextOrder);
  }

  function cancelProviderCardDrag(event: ReactPointerEvent<HTMLElement>) {
    const pending = pendingProviderCardDrag.current;
    if (pending && pending.pointerId === event.pointerId) clearProviderCardDrag(pending);
  }

  return <section className={`cloud-workspace${activePage === 'management' ? '' : ' cloud-workspace--files'}`} aria-label={t('cloud.workspace')}>
    {activePage === 'management' && <section className="cloud-management-panel" aria-label={t('cloud.management')}>
      {recoveringAccounts && <div className="cloud-account-recovery" role="status" aria-label={t('cloud.readingAccountStatus')}><i aria-hidden="true" /><span>{t('cloud.readingAccountStatus')}…</span></div>}
      {recoveryError && <p className="cloud-operation-status error" role="alert">{t('cloud.recoveryFailed', { detail: recoveryError })}</p>}
      <div className="cloud-provider-auths" onPointerMove={updateProviderCardDrag} onPointerUp={finishProviderCardDrag} onPointerCancel={cancelProviderCardDrag}>{managementCatalog.map((descriptor) => {
        const { provider } = descriptor;
        const providerName = cloudProviderLabel(provider, t);
        const failure = accountErrors[provider];
        const pending = pendingProviders.includes(provider);
        if (pending || failure?.action === 'retry') return <section key={provider} className={`cloud-manager cloud-authorization cloud-provider-${provider}`} aria-label={t('cloud.accountRecovery', { name: providerName })}>
          <header className="cloud-heading"><h2>{t('cloud.providerDrive', { name: providerName })}</h2></header>
          {pending ? <p role="status">{t('cloud.readingAccountStatus')}…</p> : <>
            <p className="cloud-operation-status error" role="alert">{failure?.message}</p>
            <button type="button" className="secondary" onClick={() => void restoreProvider(provider)}>{t('common.retryAction')}</button>
          </>}
        </section>;
        const account = accounts.find((item) => item.provider === provider);
        return <CloudManager
          key={`${provider}:${account?.id ?? 'unauthorized'}`}
          descriptor={descriptor}
          provider={provider}
          client={client}
          initialAccount={account ?? null}
          recoveryAuthorization={failure?.action}
          privacyDownloadEnabled={privacyDownloadEnabled}
          authorizationOnly
          restoreOnMount={false}
          authorizationCardDrag={{
            dragging: draggingProvider === provider,
            dropTarget: dropTargetProvider === provider,
            onPointerDown: (event) => beginProviderCardDrag(provider, event),
          }}
          onAccountAuthorized={acceptAccount}
          onAccountLoggedOut={removeAccount}
        />;
      })}</div>
    </section>}

    {/* Keep visited account workspaces alive during navigation, including their
        in-flight upload callbacks and results. Removing/replacing an account
        unmounts its workspace, so state never leaks into another login. */}
    {accounts.filter(account => visitedAccounts.has(accountKey(account)) || accountKey(account) === activeKey).map(account => <div className="cloud-file-workspace" key={`files:${accountKey(account)}`} hidden={accountKey(account) !== activeKey}>
      <CloudManager
      descriptor={catalog.find(d => d.provider === account.provider)!}
      provider={account.provider}
      client={client}
      initialAccount={account}
      workspaceVisible={accountKey(account) === activeKey}
      privacyDownloadEnabled={privacyDownloadEnabled}
      restoreOnMount={false}
      showSessionActions={false}
      onDownloadTask={onDownloadTask}
      onAccountAuthorized={acceptAccount}
      onAccountLoggedOut={removeAccount}
    /></div>)}
  </section>;
}
