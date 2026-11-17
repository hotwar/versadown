import { FormEvent, useEffect, useState } from 'react';
import { useTranslation } from '../i18n';
import type { AgentAccessSettings as AccessSettings, AppSettings } from '../lib/types';
import { AgentCloudGrants } from './AgentCloudGrants';

interface Props {
  initial?: AccessSettings;
  onSave?(settings: AccessSettings): Promise<AppSettings>;
  browseDirectory?(currentDirectory: string): Promise<string | null>;
}

export function AgentAccessSettings({ initial, onSave, browseDirectory }: Props) {
  const { t } = useTranslation();
  const [enabled, setEnabled] = useState(initial?.enabled ?? false);
  const [directories, setDirectories] = useState(initial?.allowedDirectories.join('\n') ?? '');
  const [uploadDirectories, setUploadDirectories] = useState(initial?.uploadDirectories.join('\n') ?? '');
  const [cloudGrants, setCloudGrants] = useState(initial?.cloudGrants ?? []);
  const [busy, setBusy] = useState(false);
  const [status, setStatus] = useState<{ kind: 'success' | 'error'; message: string } | null>(null);

  useEffect(() => {
    setEnabled(initial?.enabled ?? false);
    setDirectories(initial?.allowedDirectories.join('\n') ?? '');
    setUploadDirectories(initial?.uploadDirectories.join('\n') ?? '');
    setCloudGrants(initial?.cloudGrants ?? []);
  }, [initial]);

  async function browse(upload = false) {
    try {
      const selected = await browseDirectory?.((upload ? uploadDirectories : directories).split('\n')[0] ?? '');
      if (selected) {
        (upload ? setUploadDirectories : setDirectories)((current) => [...new Set([...current.split('\n').filter(Boolean), selected])].join('\n'));
        setStatus(null);
      }
    } catch {
      setStatus({ kind: 'error', message: t('settings.agent.error.invalidDirectory') });
    }
  }

  async function save(event: FormEvent) {
    event.preventDefault();
    const allowedDirectories = [...new Set(directories.split(/\r?\n/).map((entry) => entry.trim()).filter(Boolean))];
    setBusy(true);
    setStatus(null);
    try {
      if (!onSave) throw new Error(t('settings.agent.error.desktopRequired'));
      const saved = await onSave({ enabled, allowedDirectories,
        uploadDirectories: [...new Set(uploadDirectories.split(/\r?\n/).map(entry => entry.trim()).filter(Boolean))], cloudGrants });
      setEnabled(saved.agentAccess?.enabled ?? false);
      setDirectories(saved.agentAccess?.allowedDirectories.join('\n') ?? '');
      setUploadDirectories(saved.agentAccess?.uploadDirectories.join('\n') ?? '');
      setCloudGrants(saved.agentAccess?.cloudGrants ?? []);
      setStatus({ kind: 'success', message: t('settings.agent.saved') });
    } catch (error) {
      setStatus({ kind: 'error', message: error instanceof Error ? error.message : t('settings.agent.error.save') });
    } finally {
      setBusy(false);
    }
  }

  return <form className="agent-access-settings proxy-settings" onSubmit={(event) => void save(event)} aria-busy={busy}>
    <label className="setting-row"><span><b>{t('settings.agent.enabled')}</b><small>{t('settings.agent.scope')}</small></span><input type="checkbox" aria-label={t('settings.agent.enabled')} checked={enabled} disabled={busy} onChange={(event) => { setEnabled(event.target.checked); setStatus(null); }} /></label>
    <p className="agent-access-description">{t('settings.agent.description')}</p>
    <fieldset className="agent-access-fields" disabled={busy}>
      <label><span>{t('settings.agent.directories')}</span><textarea aria-label={t('settings.agent.directories')} rows={3} value={directories} onChange={(event) => { setDirectories(event.target.value); setStatus(null); }} /><small>{t('settings.agent.directoriesHint')}</small></label>
      {browseDirectory && <div className="agent-access-actions"><button type="button" onClick={() => void browse()}>{t('settings.agent.browse')}</button></div>}
      <label><span>{t('settings.agent.uploadDirectories')}</span><textarea aria-label={t('settings.agent.uploadDirectories')} rows={3} value={uploadDirectories} onChange={(event) => { setUploadDirectories(event.target.value); setStatus(null); }} /><small>{t('settings.agent.uploadDirectoriesHint')}</small></label>
      {browseDirectory && <div className="agent-access-actions"><button type="button" onClick={() => void browse(true)}>{t('settings.agent.browseUpload')}</button></div>}
      <AgentCloudGrants grants={cloudGrants} onChange={(grants) => { setCloudGrants(grants); setStatus(null); }} disabled={busy} />
      <div className="agent-access-actions">
        <button type="submit" className="primary">{busy ? t('common.saving') : t('settings.agent.save')}</button>
      </div>
    </fieldset>
    {status && <div className="settings-actions"><span role="status" className={status.kind}>{status.message}</span></div>}
  </form>;
}
