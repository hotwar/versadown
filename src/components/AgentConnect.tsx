import { useEffect, useId, useState } from 'react';
import { useTranslation } from '../i18n';

import { getAgentConnectionInfo } from '../lib/api';
import type { AgentConnectionInfo } from '../lib/types';
export type { AgentConnectionInfo } from '../lib/types';

interface AgentConnectProps {
  loadConnectionInfo?(lang: string): Promise<AgentConnectionInfo>;
}

async function copyText(value: string) {
  if ('__TAURI_INTERNALS__' in window) {
    const { writeText } = await import('@tauri-apps/plugin-clipboard-manager');
    await writeText(value);
    return;
  }
  await navigator.clipboard.writeText(value);
}

export function AgentConnect({ loadConnectionInfo = getAgentConnectionInfo }: AgentConnectProps) {
  const { t, locale } = useTranslation();
  const headingId = useId();
  const [info, setInfo] = useState<AgentConnectionInfo | null>(null);
  const [loading, setLoading] = useState(true);
  const [loadFailed, setLoadFailed] = useState(false);
  const [attempt, setAttempt] = useState(0);
  const [copying, setCopying] = useState(false);
  const [copyStatus, setCopyStatus] = useState<'prompt' | 'json' | 'error' | null>(null);
  const lang = locale === 'zh-CN' ? 'zh-CN' : 'en';

  useEffect(() => {
    let active = true;
    setInfo(null);
    setLoading(true);
    setLoadFailed(false);
    setCopyStatus(null);
    void loadConnectionInfo(lang).then((result) => {
      if (active) setInfo(result);
    }).catch(() => {
      if (active) setLoadFailed(true);
    }).finally(() => {
      if (active) setLoading(false);
    });
    return () => { active = false; };
  }, [loadConnectionInfo, lang, attempt]);

  async function copy(kind: 'prompt' | 'json') {
    if (!info?.executableExists || copying) return;
    setCopying(true);
    setCopyStatus(null);
    try {
      await copyText(kind === 'prompt' ? info.installationPrompt : info.configJson);
      setCopyStatus(kind);
    } catch {
      setCopyStatus('error');
    } finally {
      setCopying(false);
    }
  }

  return <section className="agent-connect" aria-labelledby={headingId} aria-busy={loading || copying}>
    <h4 id={headingId}>{t('agentConnect.title')}</h4>
    <p className="agent-access-description">{t('agentConnect.description')}</p>
    {loading && <p role="status">{t('agentConnect.loading')}</p>}
    {loadFailed && <p role="alert" className="error">{t('agentConnect.loadFailed')}</p>}
    {info && !info.executableExists && <p role="alert" className="error">{t('agentConnect.missingProgram')}</p>}
    <div className="agent-access-actions">
      <button type="button" className="primary" disabled={loading || copying || !info?.executableExists} onClick={() => void copy('json')}>{t('agentConnect.copyJson')}</button>
      <button type="button" className="secondary" disabled={loading || copying || !info?.executableExists} onClick={() => void copy('prompt')}>{t('agentConnect.copyPrompt')}</button>
      {(loadFailed || (info && !info.executableExists)) && <button type="button" onClick={() => setAttempt(value => value + 1)}>{t('agentConnect.retry')}</button>}
    </div>
    {copyStatus && <p role={copyStatus === 'error' ? 'alert' : 'status'} className={copyStatus === 'error' ? 'error' : 'success'}>
      {copyStatus === 'error' ? t('agentConnect.copyFailed') : copyStatus === 'prompt' ? t('agentConnect.copiedPrompt') : t('agentConnect.copiedJson')}
    </p>}
    {info && <p className="agent-access-description">{t('agentConnect.program')}: <code style={{ overflowWrap: 'anywhere' }}>{info.executablePath}</code></p>}
  </section>;
}
