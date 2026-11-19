import { useTranslation } from '../i18n';
import type { AgentCloudGrant } from '../lib/types';

interface Props {
  grants: AgentCloudGrant[];
  onChange(grants: AgentCloudGrant[]): void;
  disabled?: boolean;
}

export function AgentCloudGrants({ grants, onChange, disabled }: Props) {
  const { t } = useTranslation();
  return <section className="agent-cloud-grants" aria-label={t('settings.agent.cloudGrants')}>
    <b>{t('settings.agent.cloudGrants')}</b>
    <p className="agent-access-description">{t('settings.agent.cloudHint')}</p>
    {grants.length === 0 && <p className="agent-access-description">{t('settings.agent.noCloudGrants')}</p>}
    {grants.map((grant, index) => <div className="setting-row" key={`${grant.provider}:${grant.accountId}`}>
      <span><b>{t(`cloud.provider.${grant.provider}`)} · {grant.accountLabel || t('settings.agent.savedAccount')}</b></span>
      <button type="button" disabled={disabled} onClick={() => onChange(grants.filter((_, position) => position !== index))} aria-label={t('settings.agent.removeCloudGrant', { number: index + 1 })}>{t('settings.agent.revoke')}</button>
    </div>)}
  </section>;
}
