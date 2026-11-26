import { FormEvent, useEffect, useId, useState } from 'react';
import { useTranslation } from '../i18n';
import type { AgentAccessSettings as AccessSettings, AppSettings } from '../lib/types';
import { AgentCloudGrants } from './AgentCloudGrants';
import { AgentConnect } from './AgentConnect';

interface Props {
  initial?: AccessSettings;
  onSave?(settings: AccessSettings): Promise<AppSettings>;
}

export function AgentAccessSettings({ initial, onSave }: Props) {
  const { t } = useTranslation();
  const titleId = useId();
  const saveScopeId = useId();
  const [enabled, setEnabled] = useState(initial?.enabled ?? false);
  const [directories, setDirectories] = useState(initial?.allowedDirectories ?? []);
  const [uploadDirectories, setUploadDirectories] = useState(initial?.uploadDirectories ?? []);
  const [cloudGrants, setCloudGrants] = useState(initial?.cloudGrants ?? []);
  const [busy, setBusy] = useState(false);
  const [status, setStatus] = useState<{ kind: 'success' | 'error'; message: string } | null>(null);

  useEffect(() => {
    setEnabled(initial?.enabled ?? false);
    setDirectories(initial?.allowedDirectories ?? []);
    setUploadDirectories(initial?.uploadDirectories ?? []);
    setCloudGrants(initial?.cloudGrants ?? []);
  }, [initial]);

  async function save(event: FormEvent) {
    event.preventDefault();
    setBusy(true);
    setStatus(null);
    try {
      if (!onSave) throw new Error(t('settings.agent.error.desktopRequired'));
      const saved = await onSave({ enabled, allowedDirectories: directories, uploadDirectories, cloudGrants });
      setEnabled(saved.agentAccess?.enabled ?? false);
      setDirectories(saved.agentAccess?.allowedDirectories ?? []);
      setUploadDirectories(saved.agentAccess?.uploadDirectories ?? []);
      setCloudGrants(saved.agentAccess?.cloudGrants ?? []);
      setStatus({ kind: 'success', message: t('settings.agent.saved') });
    } catch (error) {
      setStatus({ kind: 'error', message: error instanceof Error ? error.message : t('settings.agent.error.save') });
    } finally {
      setBusy(false);
    }
  }

  return <section className="agent-access" aria-labelledby={titleId}>
    <h3 id={titleId}>{t('settings.agent.title')}</h3>
    <form className="agent-access-settings" onSubmit={(event) => void save(event)} aria-busy={busy}>
      <label className="setting-row"><span><b>{t('settings.agent.enabled')}</b><small>{t('settings.agent.scope')}</small></span><input type="checkbox" aria-label={t('settings.agent.enabled')} checked={enabled} disabled={busy} onChange={(event) => { setEnabled(event.target.checked); setStatus(null); }} /></label>
      <p className="agent-access-description">{t('settings.agent.description')}</p>
      <fieldset className="agent-access-fields" disabled={busy}>
        {directories.length > 0 && <section aria-label={t('settings.agent.directories')}>
          <b>{t('settings.agent.directories')}</b><p className="agent-access-description">{t('settings.agent.savedDirectoriesHint')}</p>
          {directories.map((path, index) => <div className="setting-row" key={path}><code>{path}</code><button type="button" aria-label={t('settings.agent.removeDownloadDirectory', { number: index + 1 })} onClick={() => { setDirectories(current => current.filter((_, position) => position !== index)); setStatus(null); }}>{t('settings.agent.revoke')}</button></div>)}
        </section>}
        {uploadDirectories.length > 0 && <section aria-label={t('settings.agent.uploadDirectories')}>
          <b>{t('settings.agent.uploadDirectories')}</b><p className="agent-access-description">{t('settings.agent.savedDirectoriesHint')}</p>
          {uploadDirectories.map((path, index) => <div className="setting-row" key={path}><code>{path}</code><button type="button" aria-label={t('settings.agent.removeUploadDirectory', { number: index + 1 })} onClick={() => { setUploadDirectories(current => current.filter((_, position) => position !== index)); setStatus(null); }}>{t('settings.agent.revoke')}</button></div>)}
        </section>}
        <AgentCloudGrants grants={cloudGrants} onChange={(grants) => { setCloudGrants(grants); setStatus(null); }} disabled={busy} />
        <div className="agent-access-save">
          <p className="agent-access-description" id={saveScopeId}>{t('settings.agent.saveScope')}</p>
          <button type="submit" className="primary" aria-describedby={saveScopeId}>{busy ? t('common.saving') : t('settings.agent.save')}</button>
        </div>
      </fieldset>
      {status && <div className="settings-actions"><span role="status" className={status.kind}>{status.message}</span></div>}
    </form>
    <AgentConnect />
  </section>;
}
