import { useEffect, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { isTauriRuntime } from '../lib/api';
import { useTranslation } from '../i18n';

export function StartupNotice() {
  const { t } = useTranslation();
  const [unavailable, setUnavailable] = useState(false);
  useEffect(() => {
    if (!isTauriRuntime()) return;
    let active = true;
    void invoke<{ credentialsUnavailable: boolean }>('startup_health').then((health) => {
      if (active) setUnavailable(health.credentialsUnavailable);
    }).catch(() => undefined);
    return () => { active = false; };
  }, []);
  return unavailable ? <div className="startup-notice" role="alert">{t('startup.credentialsUnavailable')}</div> : null;
}
