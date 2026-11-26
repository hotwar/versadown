import { useCallback, useEffect, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { useTranslation } from '../i18n';

export type InteractionStatus = 'awaiting_user' | 'awaiting_login' | 'resuming' | 'succeeded' | 'failed' | 'cancelled' | 'expired' | 'interrupted';
export interface InteractionView {
  id: string;
  status: InteractionStatus;
  kind: 'local_access' | 'cloud_account' | 'cloud_login';
  command: string;
  provider: string | null;
  accountLabel: string | null;
  downloadDirectories: string[];
  uploadFiles: string[];
  needsDirectory: boolean;
  expiresAt: number;
  errorCode: string | null;
}

const operationNames = ['add', 'batch_add', 'confirm', 'prepare', 'upload', 'create_share', 'cloud'] as const;

const terminalStatuses = new Set<InteractionStatus>(['succeeded', 'failed', 'cancelled', 'expired', 'interrupted']);

export default function AgentInteractionWindow() {
  const { t } = useTranslation();
  const [view, setView] = useState<InteractionView | null>(null);
  const [loading, setLoading] = useState(true);
  const [loadFailed, setLoadFailed] = useState(false);
  const [actionFailed, setActionFailed] = useState(false);
  const [busy, setBusy] = useState(false);
  const mounted = useRef(false);
  const busyRef = useRef(false);
  const terminal = useRef(false);
  const serial = useRef(0);

  const applyView = useCallback((result: InteractionView | null) => {
    setView(result);
    terminal.current = result === null || terminalStatuses.has(result.status);
    setLoadFailed(false);
  }, []);

  const refresh = useCallback(async () => {
    if (busyRef.current) return;
    const current = ++serial.current;
    try {
      const result = await invoke<InteractionView | null>('agent_interaction_view');
      if (mounted.current && current === serial.current) applyView(result);
    } catch {
      if (mounted.current && current === serial.current) setLoadFailed(true);
    } finally {
      if (mounted.current && current === serial.current) setLoading(false);
    }
  }, [applyView]);

  useEffect(() => {
    mounted.current = true;
    void refresh();
    let disposed = false;
    let unlisten: (() => void) | undefined;
    void listen('agent-interaction-changed', () => {
      if (disposed) return;
      terminal.current = false;
      void refresh();
    }).then(stop => { if (disposed) stop(); else unlisten = stop; }).catch(() => undefined);
    const timer = window.setInterval(() => {
      if (!terminal.current) void refresh();
    }, 1000);
    return () => {
      disposed = true;
      mounted.current = false;
      serial.current += 1;
      window.clearInterval(timer);
      unlisten?.();
    };
  }, [refresh]);

  async function act(action: 'choose' | 'allow' | 'deny') {
    if (!view || view.status !== 'awaiting_user' || busyRef.current) return;
    busyRef.current = true;
    serial.current += 1;
    setBusy(true);
    setActionFailed(false);
    try {
      if (action === 'choose') {
        const updated = await invoke<InteractionView>('agent_interaction_choose_directory', { interactionId: view.id });
        if (mounted.current) applyView(updated);
      } else {
        await invoke('agent_interaction_decide', { interactionId: view.id, allow: action === 'allow' });
        busyRef.current = false;
        await refresh();
      }
    } catch {
      if (mounted.current) setActionFailed(true);
    } finally {
      busyRef.current = false;
      if (mounted.current) setBusy(false);
    }
  }

  async function close() {
    try { await getCurrentWindow().close(); }
    catch { setActionFailed(true); }
  }

  const operation = operationNames.find(name => name === view?.command) ?? 'unknown';
  const isTerminal = view !== null && terminalStatuses.has(view.status);
  const awaitingUser = view?.status === 'awaiting_user';
  const missingDirectory = Boolean(view?.needsDirectory);

  return <main className="agent-interaction-window" style={{ padding: 24, maxWidth: 680, margin: '0 auto', overflowWrap: 'anywhere' }} aria-busy={loading || busy}>
    <h1>{t('agentInteraction.title')}</h1>
    {loading && <p role="status">{t('agentInteraction.loading')}</p>}
    {loadFailed && <p role="alert" className="error">{t('agentInteraction.loadFailed')}</p>}
    {!loading && !loadFailed && !view && <p role="status">{t('agentInteraction.noRequest')}</p>}
    {view && <>
      <p role="status">{t(`agentInteraction.status.${view.status}`)}</p>
      {view.kind === 'local_access' ? <p>{t('agentInteraction.localHint')}</p> : <p>{t('agentInteraction.cloudHint')}</p>}
      {view.kind === 'cloud_login' && <p>{t('agentInteraction.loginHint')}</p>}
      {view.kind !== 'local_access' && (view.needsDirectory || view.downloadDirectories.length > 0 || view.uploadFiles.length > 0) && <p>{t('agentInteraction.localHint')}</p>}
      <dl>
        <dt>{t('agentInteraction.operation')}</dt><dd>{t(`agentInteraction.operation.${operation}`)}</dd>
        {(view.accountLabel || view.provider) && <><dt>{t('agentInteraction.account')}</dt><dd>{view.accountLabel ?? view.provider}</dd></>}
      </dl>
      {view.downloadDirectories.length > 0 && <section><h2>{t('agentInteraction.downloadDirectories')}</h2><ul>{view.downloadDirectories.map(path => <li key={path}><code>{path}</code></li>)}</ul></section>}
      {view.uploadFiles.length > 0 && <section><h2>{t('agentInteraction.uploadFiles')}</h2><ul>{view.uploadFiles.map(path => <li key={path}><code>{path}</code></li>)}</ul></section>}
      {view.errorCode && <p>{t('agentInteraction.errorCode')}: <code>{view.errorCode}</code></p>}
      {awaitingUser && <div className="agent-access-actions">
        {view.needsDirectory && <button type="button" disabled={busy || loadFailed} onClick={() => void act('choose')}>{t('agentInteraction.chooseDirectory')}</button>}
        <button type="button" disabled={busy || loadFailed} onClick={() => void act('deny')}>{t('agentInteraction.deny')}</button>
        <button type="button" className="primary" disabled={busy || loadFailed || missingDirectory} onClick={() => void act('allow')}>{view.kind === 'cloud_login' ? t('agentInteraction.login') : t('agentInteraction.allow')}</button>
      </div>}
    </>}
    {actionFailed && <p role="alert" className="error">{t('agentInteraction.actionFailed')}</p>}
    <div className="agent-access-actions">
      {loadFailed && <button type="button" disabled={busy} onClick={() => void refresh()}>{t('agentInteraction.retry')}</button>}
      {(isTerminal || (!loading && !view)) && <button type="button" onClick={() => void close()}>{t('agentInteraction.close')}</button>}
    </div>
  </main>;
}
