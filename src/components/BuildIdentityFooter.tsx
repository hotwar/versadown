import { useEffect, useState } from 'react';
import { invoke, isTauri } from '@tauri-apps/api/core';
import { useTranslation } from '../i18n';

interface BuildInfo {
  commit: string | null;
  branch: string | null;
  dirty: boolean;
}

export function BuildIdentityFooter() {
  const { t } = useTranslation();
  const [info, setInfo] = useState<BuildInfo | null>(null);

  useEffect(() => {
    if (!isTauri()) return;
    let active = true;
    void invoke<BuildInfo>('get_build_info').then((result) => {
      if (active) setInfo(result);
    }).catch(() => undefined);
    return () => { active = false; };
  }, []);

  const description = info?.commit
    ? [t('buildInfo.commit', { commit: info.commit }), info.branch,
      info.dirty ? t('buildInfo.modified') : null].filter(Boolean).join('\n')
    : t('buildInfo.unavailable');

  return <footer className="build-identity" aria-label={t('buildInfo.label')}>
    <span title={description}>{info?.commit
      ? `${info.commit.slice(0, 7)}${info.dirty ? '*' : ''}`
      : t('buildInfo.unavailable')}</span>
  </footer>;
}
