import { FormEvent, useState } from 'react';
import { useTranslation } from '../i18n';
import type { MegaCredentials } from '../lib/cloudTypes';

/** MEGA's own sign-in form: e-mail, password and an optional two-factor code. */
export function MegaLoginForm({ onLogin }: { onLogin(credentials: MegaCredentials): Promise<void> }) {
  const { t } = useTranslation();
  const [email, setEmail] = useState('');
  const [password, setPassword] = useState('');
  const [secondFactor, setSecondFactor] = useState('');
  const [busy, setBusy] = useState(false);

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (busy || !email.trim() || !password) return;
    setBusy(true);
    try {
      await onLogin({ email: email.trim(), password, ...(secondFactor.trim() ? { secondFactor: secondFactor.trim() } : {}) });
    } finally {
      // The password never outlives the attempt in component state.
      setPassword('');
      setBusy(false);
    }
  }

  return <form className="cloud-login-form" onSubmit={submit}>
    <input aria-label={t('cloud.mega.email')} placeholder={t('cloud.mega.email')} type="email" autoComplete="username" value={email} onChange={(event) => setEmail(event.target.value)} />
    <input aria-label={t('cloud.password')} placeholder={t('cloud.password')} type="password" autoComplete="current-password" value={password} onChange={(event) => setPassword(event.target.value)} />
    <input aria-label={t('cloud.mega.secondFactor')} placeholder={t('cloud.mega.secondFactor')} inputMode="numeric" autoComplete="one-time-code" maxLength={16} value={secondFactor} onChange={(event) => setSecondFactor(event.target.value)} />
    <button className="primary" type="submit" disabled={busy}>{busy ? t('cloud.autoLogin') : t('cloud.login')}</button>
    <small className="cloud-login-note">{t('cloud.mega.sessionNote')}</small>
  </form>;
}
