import { version } from '../../src-tauri/tauri.conf.json';
import type { AppUpdate } from '../lib/types';
import { useTranslation } from '../i18n';

export type AppUpdateCheckStatus = 'idle' | 'checking' | 'upToDate' | 'available' | 'error';

export function AboutPanel({ icon, update, checkStatus = 'idle', openUpdateFailed = false, onCheckUpdate, onOpenUpdate }: {
  icon: string;
  update?: AppUpdate | null;
  checkStatus?: AppUpdateCheckStatus;
  openUpdateFailed?: boolean;
  onCheckUpdate?(): void;
  onOpenUpdate?(update: AppUpdate): void;
}) {
  const { t } = useTranslation();
  return <article className="about-panel">
    <header className="about-heading">
      <img src={icon} width="72" height="72" alt="" draggable={false} />
      <div>
        <h2>{t('app.title')}</h2>
        <span className="about-version">{t('about.versionLabel', { version })}</span>
      </div>
    </header>
    <section className="about-updates" aria-label={t('about.updatesTitle')}>
      <div className="about-update-actions">
        <button type="button" className="secondary" disabled={checkStatus === 'checking'} onClick={onCheckUpdate}>{t('about.checkUpdates')}</button>
        {update && <button type="button" className="primary" onClick={() => onOpenUpdate?.(update)}>{t('about.downloadOfficial')}</button>}
      </div>
      <div className="about-update-status" role="status">
        {checkStatus === 'checking' && <p>{t('about.checkingUpdates')}</p>}
        {checkStatus === 'upToDate' && <p>{t('about.noUpdate')}</p>}
        {checkStatus === 'error' && <p className="about-update-error">{t('about.checkUpdateFailed')}</p>}
        {update && <div className="about-update">
          <strong>{t('about.newVersion', { version: update.version })}</strong>
          <p>{update.notes || t('about.updateNotesFallback')}</p>
        </div>}
      </div>
      {openUpdateFailed && <p className="about-update-error" role="alert">{t('about.openUpdateFailed')}</p>}
    </section>
    <p className="about-intro">{t('about.intro')}</p>
    <section className="about-section" aria-labelledby="about-features-title">
      <h3 id="about-features-title">{t('about.features.title')}</h3>
      <dl className="about-features">
        <div><dt>{t('about.features.protocols.title')}</dt><dd>{t('about.features.protocols.body')}</dd></div>
        <div><dt>{t('about.features.privacy.title')}</dt><dd>{t('about.features.privacy.body')}</dd></div>
        <div><dt>{t('about.features.cloud.title')}</dt><dd>{t('about.features.cloud.body')}</dd></div>
        <div><dt>{t('about.features.smart.title')}</dt><dd>{t('about.features.smart.body')}</dd></div>
      </dl>
    </section>
    <section className="about-section" aria-labelledby="about-help-title">
      <h3 id="about-help-title">{t('about.instructions.title')}</h3>
      <ol className="about-instructions">
        <li>{t('about.instructions.one')}</li>
        <li>{t('about.instructions.two')}</li>
        <li>{t('about.instructions.three')}</li>
      </ol>
    </section>
    <section className="about-section" aria-labelledby="about-thanks-title">
      <h3 id="about-thanks-title">{t('about.thanks.title')}</h3>
      <p className="about-thanks">{t('about.thanks.aria2')} <a href="https://aria2.github.io/" target="_blank" rel="noreferrer">aria2</a> {t('about.thanks.and')} <a href="https://github.com/AnInsomniacy/aria2-next" target="_blank" rel="noreferrer">aria2-next</a>{t('about.thanks.aria2End')}</p>
      <p className="about-thanks">{t('about.thanks.icons')} <a href="https://lucide.dev/" target="_blank" rel="noreferrer">Lucide</a>.</p>
    </section>
    <section className="about-section" aria-labelledby="about-declaration-title">
      <h3 id="about-declaration-title">{t('about.legal.title')}</h3>
      <ol className="about-instructions">
        {Array.from({ length: 10 }, (_, index) => <li key={index}>{t(`about.legal.${index + 1}` as Parameters<typeof t>[0])}</li>)}
      </ol>
      <p className="about-declaration-acceptance">{t('about.legal.acceptance')}</p>
    </section>
  </article>;
}
