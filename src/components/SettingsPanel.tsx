import { FormEvent, useEffect, useRef, useState, useSyncExternalStore } from 'react';
import { AiSettings, AiSettingsInput, AppSettings, Aria2Settings, LauncherVisibility, MagnetMetadataSettings, ProxyProtocol } from '../lib/types';
import { getThemeMode, setThemeMode, subscribeTheme, themeOptions, ThemeMode } from '../lib/theme';
import { localeMetadata, useTranslation } from '../i18n';
import { videoMessage } from '../lib/webVideo';

type SettingsTab = 'display' | 'network' | 'advanced';

const tabs: ReadonlyArray<{ id: SettingsTab }> = [
  { id: 'display' },
  { id: 'network' },
  { id: 'advanced' }
];

interface SettingsPanelProps {
  initial: AppSettings;
  onChange(settings: AppSettings): void | Promise<void>;
  browseDirectory?(currentDirectory: string): Promise<string | null>;
  browseVideoCookieFile?(currentFile: string): Promise<string | null>;
  saveAiSettings?(settings: AiSettingsInput): Promise<AiSettings>;
  loadAiModels?(settings: AiSettingsInput): Promise<string[]>;
  onNotice?(notice: { kind: 'success' | 'error'; message: string }): void;
  configurePrivacyDownload?(enabled: boolean, currentPassword?: string, newPassword?: string): Promise<AppSettings>;
}

const disabledAiSettings: AiSettings = { enabled: false, baseUrl: '', model: '', apiKeyConfigured: false };
const defaultMagnetMetadataSettings: MagnetMetadataSettings = { enabled: true, baseUrl: 'http://peer.aigutta.com:8080' };
const defaultAria2Settings: Aria2Settings = { rpcPort: 6800, rpcToken: '', maxConcurrentDownloads: 20, maxConnectionsPerServer: 4, btSeedRatio: 1.5, debugLogging: false, overallDownloadLimit: 0, p2pUploadLimit: 1048576 };
const mebibyte = 1024 * 1024;
type RateLimitInput = 'overallDownloadLimit' | 'p2pUploadLimit' | 'cloudUploadLimit';

function formatRateLimit(bytes: number) {
  return (bytes / mebibyte).toFixed(1);
}

function toRateLimitBytes(value: string) {
  if (!value || !/^\d*(?:\.\d?)?$/.test(value)) return null;
  const amount = Number(value);
  return Number.isFinite(amount) && amount >= 0 ? Math.round(amount * mebibyte) : null;
}

function createRateLimitInputs(aria2: Aria2Settings, cloudUploadLimit: number) {
  return {
    overallDownloadLimit: formatRateLimit(aria2.overallDownloadLimit),
    p2pUploadLimit: formatRateLimit(aria2.p2pUploadLimit),
    cloudUploadLimit: formatRateLimit(cloudUploadLimit)
  };
}

function PasswordVisibilityIcon({ visible }: { visible: boolean }) {
  return visible
    ? <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M3 3l18 18M10.6 10.6a2 2 0 0 0 2.8 2.8M9.9 4.3A10.8 10.8 0 0 1 12 4c5 0 8.3 4.1 9 8-.3 1.5-1.1 3-2.4 4.2M6.1 6.1C4.4 7.6 3.4 9.8 3 12c.7 3.9 4 8 9 8 1.2 0 2.3-.2 3.3-.7" /></svg>
    : <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M3 12s3.2-8 9-8 9 8 9 8-3.2 8-9 8-9-8-9-8Z" /><circle cx="12" cy="12" r="3" /></svg>;
}

export function SettingsPanel({ initial, onChange, browseDirectory, browseVideoCookieFile, saveAiSettings, loadAiModels, onNotice, configurePrivacyDownload }: SettingsPanelProps) {
  const { t, localePreference, setLocalePreference } = useTranslation();
  const [downloadDirectory, setDownloadDirectory] = useState(initial.downloadDirectory);
  const [launcherVisibility, setLauncherVisibility] = useState<LauncherVisibility>(initial.launcherVisibility);
  const [proxy, setProxy] = useState(initial.proxy);
  const [videoCookieFile, setVideoCookieFile] = useState(initial.videoCookieFile ?? '');
  const [savedVideoCookieFile, setSavedVideoCookieFile] = useState(initial.videoCookieFile);
  const [videoCookieBusy, setVideoCookieBusy] = useState(false);
  const [videoCookieStatus, setVideoCookieStatus] = useState<{ kind: 'success' | 'error'; message: string } | null>(null);
  const [ai, setAi] = useState<AiSettings>(initial.ai ?? disabledAiSettings);
  const magnetMetadata: MagnetMetadataSettings = initial.magnetMetadata ?? defaultMagnetMetadataSettings;
  const [aria2, setAria2] = useState<Aria2Settings>({ ...defaultAria2Settings, ...initial.aria2 });
  const [cloudUploadLimit, setCloudUploadLimit] = useState(initial.cloudUploadLimit ?? 0);
  const [rateLimitInputs, setRateLimitInputs] = useState(() => createRateLimitInputs({ ...defaultAria2Settings, ...initial.aria2 }, initial.cloudUploadLimit ?? 0));
  const [deleteTransferredShareFilesAfterDownload, setDeleteTransferredShareFilesAfterDownload] = useState(initial.deleteTransferredShareFilesAfterDownload ?? true);
  const [downloadStatusNotifications, setDownloadStatusNotifications] = useState(initial.downloadStatusNotifications ?? true);
  const [associateDownloadLinks, setAssociateDownloadLinks] = useState(initial.associateDownloadLinks ?? true);
  const [privacyDownloadEnabled, setPrivacyDownloadEnabled] = useState(initial.privacyDownloadEnabled ?? false);
  const [privacyPasswordConfigured, setPrivacyPasswordConfigured] = useState(initial.privacyPasswordConfigured ?? false);
  const [privacyCurrentPassword, setPrivacyCurrentPassword] = useState('');
  const [privacyNewPassword, setPrivacyNewPassword] = useState('');
  const [privacyPasswordConfirmation, setPrivacyPasswordConfirmation] = useState('');
  const [privacyStatus, setPrivacyStatus] = useState<{ kind: 'success' | 'error'; message: string } | null>(null);
  const [aria2Status, setAria2Status] = useState<{ kind: 'success' | 'error'; message: string } | null>(null);
  const [showRpcToken, setShowRpcToken] = useState(false);
  const [rateLimitStatus, setRateLimitStatus] = useState<{ kind: 'success' | 'error'; message: string } | null>(null);
  const [aiApiKey, setAiApiKey] = useState('');
  const [aiModels, setAiModels] = useState<string[]>([]);
  const [aiModelError, setAiModelError] = useState<string | null>(null);
  const [aiStatus, setAiStatus] = useState<{ kind: 'success' | 'error'; message: string } | null>(null);
  const [aiBusy, setAiBusy] = useState(false);
  const aiModelRequestInFlight = useRef(false);
  const aiModelSelect = useRef<HTMLSelectElement>(null);
  const [saved, setSaved] = useState(false);
  const [activeTab, setActiveTab] = useState<SettingsTab>('display');
  const themeMode = useSyncExternalStore(subscribeTheme, getThemeMode, getThemeMode);

  useEffect(() => {
    setVideoCookieFile(initial.videoCookieFile ?? '');
    setSavedVideoCookieFile(initial.videoCookieFile);
  }, [initial.videoCookieFile]);

  useEffect(() => { const nextAria2 = { ...defaultAria2Settings, ...initial.aria2 }; const nextCloudUploadLimit = initial.cloudUploadLimit ?? 0; setDownloadDirectory(initial.downloadDirectory); setLauncherVisibility(initial.launcherVisibility); setProxy(initial.proxy); setAi(initial.ai ?? disabledAiSettings); setAria2(nextAria2); setCloudUploadLimit(nextCloudUploadLimit); setRateLimitInputs(createRateLimitInputs(nextAria2, nextCloudUploadLimit)); setDeleteTransferredShareFilesAfterDownload(initial.deleteTransferredShareFilesAfterDownload ?? true); setDownloadStatusNotifications(initial.downloadStatusNotifications ?? true); setAssociateDownloadLinks(initial.associateDownloadLinks ?? true); setPrivacyDownloadEnabled(initial.privacyDownloadEnabled ?? false); setPrivacyPasswordConfigured(initial.privacyPasswordConfigured ?? false); }, [initial]);

  function currentSettings(overrides: Partial<AppSettings> = {}): AppSettings {
    return { downloadDirectory, launcherVisibility, proxy, ...(savedVideoCookieFile !== undefined ? { videoCookieFile: savedVideoCookieFile } : {}), ai, magnetMetadata, aria2, ...(cloudUploadLimit || initial.cloudUploadLimit !== undefined ? { cloudUploadLimit } : {}), deleteTransferredShareFilesAfterDownload, ...(initial.downloadStatusNotifications !== undefined ? { downloadStatusNotifications } : {}), ...(initial.associateDownloadLinks !== undefined ? { associateDownloadLinks } : {}), ...overrides };
  }

  function saveWithoutStatus(settings: AppSettings) {
    void Promise.resolve(onChange(settings)).catch(() => undefined);
  }

  const currentAiInput = (): AiSettingsInput => ({
    enabled: ai.enabled,
    baseUrl: ai.baseUrl.trim(),
    model: ai.model.trim(),
    apiKey: aiApiKey.trim()
  });

  function notifyAi(notice: { kind: 'success' | 'error'; message: string }) {
    setAiStatus(notice);
    onNotice?.(notice);
  }

  function updateProxy<Key extends keyof typeof proxy>(key: Key, value: (typeof proxy)[Key]) {
    setSaved(false);
    setProxy((current) => ({ ...current, [key]: value }));
  }

  function saveProxy(event: FormEvent) {
    event.preventDefault();
    saveWithoutStatus(currentSettings({ proxy: { ...proxy, host: proxy.host.trim(), username: proxy.username.trim() } }));
    setSaved(true);
  }

  async function saveVideoCookieFile(path: string) {
    const normalized = path.trim();
    setVideoCookieStatus(null);
    setVideoCookieBusy(true);
    try {
      await onChange(currentSettings({ videoCookieFile: normalized }));
      setVideoCookieFile(normalized);
      setSavedVideoCookieFile(normalized);
      setVideoCookieStatus({ kind: 'success', message: t('settings.videoCookies.saved') });
    } catch (error) {
      setVideoCookieStatus({ kind: 'error', message: videoMessage(error) });
    } finally {
      setVideoCookieBusy(false);
    }
  }

  async function chooseVideoCookieFile() {
    setVideoCookieStatus(null);
    setVideoCookieBusy(true);
    try {
      const selected = await browseVideoCookieFile?.(videoCookieFile);
      if (selected) await saveVideoCookieFile(selected);
    } catch (error) {
      setVideoCookieStatus({ kind: 'error', message: videoMessage(error) });
    } finally {
      setVideoCookieBusy(false);
    }
  }

  function saveDownloadDirectory(event: FormEvent) {
    event.preventDefault();
    commitDownloadDirectory(downloadDirectory);
  }

  function commitDownloadDirectory(directory: string) {
    const normalized = directory.trim();
    if (!normalized) return;
    setDownloadDirectory(normalized);
    saveWithoutStatus(currentSettings({ downloadDirectory: normalized }));
  }

  function updateLauncherVisibility(visibility: LauncherVisibility) {
    setLauncherVisibility(visibility);
    saveWithoutStatus(currentSettings({ launcherVisibility: visibility }));
  }

  function updateThemeMode(mode: ThemeMode) {
    setThemeMode(mode);
  }

  async function reloadAiModels() {
    if (aiModelRequestInFlight.current) return;
    const configuration = currentAiInput();
    setAiStatus(null);
    setAiModelError(null);
    const showModelError = (message: string) => {
      aiModelSelect.current?.blur();
      setAiModelError(message);
    };
    if (!configuration.baseUrl) {
      showModelError(t('settings.ai.addressRequired'));
      return;
    }
    if (!configuration.apiKey && !ai.apiKeyConfigured) {
      showModelError(t('settings.ai.keyRequired'));
      return;
    }
    aiModelRequestInFlight.current = true;
    setAiBusy(true);
    try {
      const models = await loadAiModels?.(configuration) ?? [];
      if (!models.length) throw new Error(t('settings.ai.noModels'));
      setAiModels(models);
    } catch (error) {
      const detail = error instanceof Error ? error.message : String(error);
      showModelError(t('settings.ai.fetchModelsFailed', { detail }));
    } finally {
      aiModelRequestInFlight.current = false;
      setAiBusy(false);
    }
  }

  async function selectAiModel(model: string) {
    setAiModelError(null);
    if (!model) {
      setAi((current) => ({ ...current, model }));
      return;
    }
    const previous = ai;
    const configuration = { ...currentAiInput(), model };
    setAi((current) => ({ ...current, model }));
    setAiStatus(null);
    setAiBusy(true);
    try {
      const savedAi = await saveAiSettings?.(configuration) ?? { ...ai, model, apiKeyConfigured: ai.apiKeyConfigured || Boolean(aiApiKey.trim()) };
      setAi(savedAi);
      setAiApiKey('');
      notifyAi({ kind: 'success', message: t('settings.ai.saved') });
    } catch (error) {
      setAi(previous);
      const detail = error instanceof Error ? error.message : String(error);
      notifyAi({ kind: 'error', message: t('settings.ai.saveFailed', { detail }) });
    } finally {
      setAiBusy(false);
    }
  }

  function updateTransferredShareCleanup(enabled: boolean) {
    setDeleteTransferredShareFilesAfterDownload(enabled);
    saveWithoutStatus(currentSettings({ deleteTransferredShareFilesAfterDownload: enabled }));
  }

  function updateDownloadStatusNotifications(enabled: boolean) {
    setDownloadStatusNotifications(enabled);
    saveWithoutStatus(currentSettings({ downloadStatusNotifications: enabled }));
  }

  function updateAssociateDownloadLinks(enabled: boolean) {
    setAssociateDownloadLinks(enabled);
    void Promise.resolve(onChange(currentSettings({ associateDownloadLinks: enabled }))).catch((error: unknown) => {
      setAssociateDownloadLinks(!enabled);
      onNotice?.({ kind: 'error', message: error instanceof Error ? error.message : String(error) });
    });
  }

  async function updatePrivacyDownload(enabled: boolean) {
    const previous = privacyDownloadEnabled;
    setPrivacyDownloadEnabled(enabled);
    setPrivacyStatus(null);
    if (!privacyPasswordConfigured) return;
    try {
      const saved = await configurePrivacyDownload?.(enabled)
        ?? currentSettings({ privacyDownloadEnabled: enabled, privacyPasswordConfigured: true });
      setPrivacyDownloadEnabled(saved.privacyDownloadEnabled ?? enabled);
      setPrivacyPasswordConfigured(saved.privacyPasswordConfigured ?? true);
      setPrivacyStatus(null);
    } catch (error) {
      setPrivacyDownloadEnabled(previous);
      setPrivacyStatus({ kind: 'error', message: error instanceof Error ? error.message : String(error) });
    }
  }

  async function savePrivacyDownload(event: FormEvent) {
    event.preventDefault();
    setPrivacyStatus(null);
    if (!privacyPasswordConfigured && privacyDownloadEnabled && !privacyNewPassword) {
      setPrivacyStatus({ kind: 'error', message: t('settings.privacy.passwordMissing') });
      return;
    }
    if (!privacyPasswordConfigured && privacyNewPassword !== privacyPasswordConfirmation) {
      setPrivacyStatus({ kind: 'error', message: t('settings.privacy.passwordMismatch') });
      return;
    }
    if (privacyPasswordConfigured && !privacyCurrentPassword) {
      setPrivacyStatus({ kind: 'error', message: t('settings.privacy.currentPasswordMissing') });
      return;
    }
    try {
      const saved = await configurePrivacyDownload?.(
        privacyDownloadEnabled,
        privacyPasswordConfigured ? privacyCurrentPassword : undefined,
        privacyNewPassword || undefined
      ) ?? currentSettings({ privacyDownloadEnabled, privacyPasswordConfigured: privacyPasswordConfigured || Boolean(privacyNewPassword) });
      setPrivacyPasswordConfigured(saved.privacyPasswordConfigured ?? Boolean(privacyNewPassword));
      setPrivacyCurrentPassword('');
      setPrivacyNewPassword('');
      setPrivacyPasswordConfirmation('');
      setPrivacyStatus(null);
    } catch (error) {
      setPrivacyStatus({ kind: 'error', message: error instanceof Error ? error.message : String(error) });
    }
  }

  function updateAria2<Key extends keyof Aria2Settings>(key: Key, value: Aria2Settings[Key]) {
    setAria2Status(null);
    setAria2((current) => ({ ...current, [key]: value }));
  }

  async function updateDebugLogging(enabled: boolean) {
    const previous = aria2;
    const next = { ...aria2, debugLogging: enabled };
    setAria2(next);
    setAria2Status(null);
    try {
      await onChange(currentSettings({ aria2: next }));
    } catch (error) {
      setAria2(previous);
      setAria2Status({ kind: 'error', message: error instanceof Error ? error.message : String(error) });
    }
  }

  async function saveAria2(event: FormEvent) {
    event.preventDefault();
    const savedAria2 = { ...aria2, rpcToken: aria2.rpcToken.trim() };
    setAria2(savedAria2);
    setAria2Status(null);
    try {
      await onChange(currentSettings({ aria2: savedAria2 }));
      setAria2Status({ kind: 'success', message: t('settings.savedApplied') });
    } catch (error) {
      setAria2Status({ kind: 'error', message: error instanceof Error ? error.message : String(error) });
    }
  }

  function updateRateLimitInput(key: RateLimitInput, value: string) {
    if (!/^\d*(?:\.\d?)?$/.test(value)) return;
    setRateLimitStatus(null);
    setRateLimitInputs((current) => ({ ...current, [key]: value }));
    const bytes = toRateLimitBytes(value);
    if (bytes === null) return;
    if (key === 'cloudUploadLimit') {
      setCloudUploadLimit(bytes);
      return;
    }
    setAria2((current) => ({ ...current, [key]: bytes }));
  }

  function normalizeRateLimitInput(key: RateLimitInput) {
    const bytes = toRateLimitBytes(rateLimitInputs[key]) ?? (key === 'cloudUploadLimit' ? cloudUploadLimit : aria2[key]);
    setRateLimitInputs((current) => ({ ...current, [key]: formatRateLimit(bytes) }));
  }

  async function saveRateLimits(event: FormEvent) {
    event.preventDefault();
    const savedAria2 = {
      ...aria2,
      rpcToken: aria2.rpcToken.trim(),
      overallDownloadLimit: toRateLimitBytes(rateLimitInputs.overallDownloadLimit) ?? aria2.overallDownloadLimit,
      p2pUploadLimit: toRateLimitBytes(rateLimitInputs.p2pUploadLimit) ?? aria2.p2pUploadLimit
    };
    const savedCloudUploadLimit = toRateLimitBytes(rateLimitInputs.cloudUploadLimit) ?? cloudUploadLimit;
    setAria2(savedAria2);
    setCloudUploadLimit(savedCloudUploadLimit);
    setRateLimitInputs(createRateLimitInputs(savedAria2, savedCloudUploadLimit));
    setRateLimitStatus(null);
    try {
      await onChange(currentSettings({ aria2: savedAria2, cloudUploadLimit: savedCloudUploadLimit }));
      setRateLimitStatus({ kind: 'success', message: t('settings.savedApplied') });
    } catch (error) {
      setRateLimitStatus({ kind: 'error', message: error instanceof Error ? error.message : String(error) });
    }
  }

  async function browseForDownloadDirectory() {
    const selected = await browseDirectory?.(downloadDirectory);
    if (selected) commitDownloadDirectory(selected);
  }

  return <section className="settings-panel" aria-label={t('settings.title')}>
    <div className="settings-tabs workspace-tab-list" role="tablist" aria-label={t('settings.categories')}>
      {tabs.map((tab) => <button key={tab.id} role="tab" aria-selected={activeTab === tab.id} onClick={() => setActiveTab(tab.id)}>{t(`settings.tab.${tab.id === 'display' ? 'general' : tab.id}` as 'settings.tab.general' | 'settings.tab.network' | 'settings.tab.advanced')}</button>)}
    </div>
    <div className="settings-content">
      {activeTab === 'display' && <>
        <div className="settings-display-preferences">
          <label className="setting-row"><span><b>{t('settings.language.label')}</b></span><select aria-label={t('settings.language.label')} value={localePreference} onChange={(event) => void setLocalePreference(event.target.value as typeof localePreference)}><option value="auto">{t('common.automatic')}</option>{localeMetadata.map((locale) => <option key={locale.id} value={locale.id}>{locale.label}</option>)}</select></label>
          <label className="setting-row"><span><b>{t('settings.launcher.visibility')}</b></span><select aria-label={t('settings.launcher.visibility')} value={launcherVisibility} onChange={(event) => updateLauncherVisibility(event.target.value as LauncherVisibility)}><option value="always">{t('settings.launcher.always')}</option><option value="downloading">{t('settings.launcher.downloading')}</option><option value="hidden">{t('settings.launcher.hidden')}</option></select></label>
        </div>
        <section className="theme-setting" aria-labelledby="theme-setting-title">
          <header><span><b id="theme-setting-title">{t('settings.appearance')}</b></span></header>
          <div className="theme-picker" role="radiogroup" aria-label={t('settings.appearance')}>
            <button type="button" role="radio" aria-checked={themeMode === 'auto'} aria-label={t('settings.theme.followSystemAria')} className="theme-option theme-option-auto" onClick={() => updateThemeMode('auto')}>
              <span className="theme-preview" aria-hidden="true"><i /><i /><i /></span>
              <span><b>{t('settings.theme.followSystem')}</b></span>
              <i className="theme-check" aria-hidden="true">✓</i>
            </button>
            {themeOptions.map((theme) => {
              const themeLabel = t(({ 'clear-blue': 'settings.theme.clearBlue', mint: 'settings.theme.mint', 'warm-gold': 'settings.theme.warmGold', lavender: 'settings.theme.lavender', coral: 'settings.theme.coral', 'deep-ocean': 'settings.theme.deepOcean', 'night-violet': 'settings.theme.nightViolet' } as const)[theme.id]);
              return <button key={theme.id} type="button" role="radio" aria-checked={themeMode === theme.id} aria-label={`${themeLabel}${t('settings.theme.suffix')}`} className="theme-option" onClick={() => updateThemeMode(theme.id)}>
                <span className="theme-preview" aria-hidden="true">{theme.preview.map((color) => <i key={color} style={{ background: color }} />)}</span>
                <span><b>{themeLabel}</b></span>
                <i className="theme-check" aria-hidden="true">✓</i>
              </button>;
            })}
          </div>
        </section>
        <form className="directory-settings" onSubmit={saveDownloadDirectory}>
          <label><span><b>{t('settings.directory.label')}</b></span><input aria-label={t('settings.directory.label')} required value={downloadDirectory} placeholder="C:\\Users\\Username\\Downloads" onChange={(event) => setDownloadDirectory(event.target.value)} onBlur={(event) => commitDownloadDirectory(event.target.value)} /></label>
          <button className="secondary" type="button" aria-label={t('settings.directory.browse')} onClick={() => void browseForDownloadDirectory()}>{t('settings.directory.browse')}</button>
        </form>
        <label className="setting-row"><span><b>{t('settings.transferredCleanup')}</b></span><input aria-label={t('settings.transferredCleanup')} type="checkbox" checked={deleteTransferredShareFilesAfterDownload} onChange={(event) => updateTransferredShareCleanup(event.target.checked)} /></label>
        <label className="setting-row"><span><b>{t('settings.downloadNotifications')}</b></span><input aria-label={t('settings.downloadNotifications')} type="checkbox" checked={downloadStatusNotifications} onChange={(event) => updateDownloadStatusNotifications(event.target.checked)} /></label>
        <label className="setting-row"><span><b>{t('settings.associateDownloadLinks')}</b></span><input aria-label={t('settings.associateDownloadLinks')} type="checkbox" checked={associateDownloadLinks} onChange={(event) => updateAssociateDownloadLinks(event.target.checked)} /></label>
      </>}
      {activeTab === 'network' && <>
      <form className="proxy-settings" onSubmit={saveProxy}>
        <div className="setting-row"><span><b>{t('settings.enableProxy')}</b></span><input aria-label={t('settings.enableProxyAria')} type="checkbox" checked={proxy.enabled} onChange={(event) => updateProxy('enabled', event.target.checked)} /></div>
        <fieldset aria-disabled={!proxy.enabled}>
          <label><span>{t('settings.proxy.protocol')}</span><select aria-label={t('settings.proxy.protocol')} disabled={!proxy.enabled} value={proxy.protocol} onChange={(event) => updateProxy('protocol', event.target.value as ProxyProtocol)}><option value="http">HTTP</option><option value="https">HTTPS</option></select></label>
          <label className="proxy-host"><span>{t('settings.proxy.server')}</span><input aria-label={t('settings.proxy.server')} disabled={!proxy.enabled} required value={proxy.host} placeholder="127.0.0.1" onChange={(event) => updateProxy('host', event.target.value)} /></label>
          <label><span>{t('settings.proxy.port')}</span><input aria-label={t('settings.proxy.port')} disabled={!proxy.enabled} required type="number" min="1" max="65535" value={proxy.port} onChange={(event) => updateProxy('port', Number(event.target.value))} /></label>
          <label><span>{t('settings.proxy.usernameOptional')}</span><input aria-label={t('settings.proxy.usernameOptional')} disabled={!proxy.enabled} value={proxy.username} autoComplete="off" onChange={(event) => updateProxy('username', event.target.value)} /></label>
          <label><span>{t('settings.proxy.passwordOptional')}</span><input aria-label={t('settings.proxy.passwordOptional')} disabled={!proxy.enabled} type="password" value={proxy.password} autoComplete="new-password" onChange={(event) => updateProxy('password', event.target.value)} /></label>
          <button className="primary settings-inline-save" type="submit">{t('settings.proxy.save')}</button>
        </fieldset>
        {saved && <div className="settings-actions"><span role="status">{t('settings.savedNewTasks')}</span></div>}
      </form>
      <form className="proxy-settings video-cookie-settings" onSubmit={event => { event.preventDefault(); void saveVideoCookieFile(videoCookieFile); }}>
        <div className="setting-row"><span><b>{t('settings.videoCookies.title')}</b><small id="video-cookie-description">{t('settings.videoCookies.description')}</small></span></div>
        <fieldset disabled={videoCookieBusy}>
          <label><span>{t('settings.videoCookies.path')}</span><input aria-label={t('settings.videoCookies.path')} aria-describedby="video-cookie-description" autoComplete="off" spellCheck={false} value={videoCookieFile} placeholder={t('settings.videoCookies.placeholder')} onChange={event => { setVideoCookieFile(event.target.value); setVideoCookieStatus(null); }} /></label>
          <div className="video-cookie-actions">
            <button className="secondary" type="button" disabled={!browseVideoCookieFile} onClick={() => void chooseVideoCookieFile()}>{t('settings.videoCookies.choose')}</button>
            <button className="secondary" type="button" disabled={!videoCookieFile && !savedVideoCookieFile} onClick={() => void saveVideoCookieFile('')}>{t('settings.videoCookies.clear')}</button>
            <button className="primary settings-inline-save" type="submit">{t('settings.videoCookies.save')}</button>
          </div>
        </fieldset>
        {videoCookieStatus && <div className="settings-actions"><span role="status" className={videoCookieStatus.kind}>{videoCookieStatus.message}</span></div>}
      </form>
      <form className="proxy-settings rate-limit-settings" onSubmit={saveRateLimits}>
        <div className="setting-row rate-limit-heading"><span><b>{t('settings.rateLimit')}</b><small>{t('settings.unlimitedHint')}</small></span></div>
        <fieldset>
          <label><span>{t('settings.overallDownloadLimit')}</span><input aria-label={t('settings.overallDownloadLimit')} type="number" min="0" step="0.1" required value={rateLimitInputs.overallDownloadLimit} onChange={(event) => updateRateLimitInput('overallDownloadLimit', event.target.value)} onBlur={() => normalizeRateLimitInput('overallDownloadLimit')} /></label>
          <label><span>{t('settings.p2pUploadLimit')}</span><input aria-label={t('settings.p2pUploadLimit')} type="number" min="0" step="0.1" required value={rateLimitInputs.p2pUploadLimit} onChange={(event) => updateRateLimitInput('p2pUploadLimit', event.target.value)} onBlur={() => normalizeRateLimitInput('p2pUploadLimit')} /></label>
          <label><span>{t('settings.cloudUploadLimit')}</span><input aria-label={t('settings.cloudUploadLimit')} type="number" min="0" step="0.1" required value={rateLimitInputs.cloudUploadLimit} onChange={(event) => updateRateLimitInput('cloudUploadLimit', event.target.value)} onBlur={() => normalizeRateLimitInput('cloudUploadLimit')} /></label>
          <button className="primary settings-inline-save" type="submit">{t('settings.rateLimit.save')}</button>
        </fieldset>
        {rateLimitStatus && <div className="settings-actions"><span role="status" className={rateLimitStatus.kind}>{rateLimitStatus.message}</span></div>}
      </form>
      <form className="proxy-settings aria2-settings" onSubmit={saveAria2}>
        <div className="setting-row"><span><b>{t('settings.aria2.title')}</b></span></div>
        <fieldset>
          <label><span>{t('settings.aria2.rpcPort')}</span><input aria-label={t('settings.aria2.rpcPort')} required type="number" min="1024" max="65535" value={aria2.rpcPort} onChange={(event) => updateAria2('rpcPort', Number(event.target.value))} /></label>
          <label className="aria2-token"><span>{t('settings.aria2.rpcTokenOptional')}</span><div className="secret-input"><input aria-label={t('settings.aria2.rpcToken')} type={showRpcToken ? 'text' : 'password'} autoComplete="new-password" value={aria2.rpcToken} onChange={(event) => updateAria2('rpcToken', event.target.value)} /><button type="button" aria-label={showRpcToken ? t('settings.aria2.hideToken') : t('settings.aria2.showToken')} title={showRpcToken ? t('settings.aria2.hideToken') : t('settings.aria2.showToken')} onClick={() => setShowRpcToken((visible) => !visible)}><PasswordVisibilityIcon visible={showRpcToken} /></button></div></label>
          <label><span>{t('settings.aria2.maxTasks')}</span><input aria-label={t('settings.aria2.maxTasks')} required type="number" min="1" max="64" value={aria2.maxConcurrentDownloads} onChange={(event) => updateAria2('maxConcurrentDownloads', Number(event.target.value))} /></label>
          <label><span>{t('settings.aria2.maxConnections')}</span><input aria-label={t('settings.aria2.maxConnections')} required type="number" min="1" max="16" value={aria2.maxConnectionsPerServer} onChange={(event) => updateAria2('maxConnectionsPerServer', Number(event.target.value))} /></label>
          <label className="aria2-seed-ratio"><span>{t('settings.aria2.seedRatio')}</span><input aria-label={t('settings.aria2.seedRatio')} required type="number" min="0.1" max="1000" step="0.1" value={aria2.btSeedRatio} onChange={(event) => updateAria2('btSeedRatio', Number(event.target.value))} /></label>
          <button className="primary settings-inline-save" type="submit">{t('settings.aria2.save')}</button>
        </fieldset>
        {aria2Status && <div className="settings-actions"><span role="status" className={aria2Status.kind}>{aria2Status.message}</span></div>}
      </form>
      </>}
      {activeTab === 'advanced' && <>
        <section className="aria2-debug-settings proxy-settings">
          <label className="setting-row"><span><b>{t('settings.debugLogging')}</b><small>{t('settings.debugLogging.description')}</small></span><input aria-label={t('settings.debugLogging')} type="checkbox" checked={aria2.debugLogging} onChange={(event) => void updateDebugLogging(event.target.checked)} /></label>
          {aria2Status?.kind === 'error' && <div className="settings-actions"><span role="status" className="error">{aria2Status.message}</span></div>}
        </section>
        <form className="privacy-download-settings proxy-settings" onSubmit={(event) => void savePrivacyDownload(event)}>
          <label className="setting-row"><span><b>{t('settings.privacy.enabled')}</b><small>{t('settings.privacy.description')}</small></span><input aria-label={t('settings.privacy.enabled')} type="checkbox" checked={privacyDownloadEnabled} onChange={(event) => void updatePrivacyDownload(event.target.checked)} /></label>
          <fieldset className="privacy-password-fields" disabled={!privacyDownloadEnabled}>
            {privacyPasswordConfigured && <label><span>{t('settings.privacy.currentPassword')}</span><input aria-label={t('settings.privacy.currentPassword')} type="password" autoComplete="current-password" value={privacyCurrentPassword} onChange={(event) => setPrivacyCurrentPassword(event.target.value)} /></label>}
            <label><span>{t('settings.privacy.newPassword')}</span><input aria-label={t('settings.privacy.newPassword')} type="password" autoComplete="new-password" placeholder={privacyPasswordConfigured ? t('settings.privacy.keepPassword') : t('settings.privacy.passwordPlaceholder')} value={privacyNewPassword} onChange={(event) => setPrivacyNewPassword(event.target.value)} /></label>
            {!privacyPasswordConfigured && <label><span>{t('settings.privacy.confirmPassword')}</span><input aria-label={t('settings.privacy.confirmPassword')} type="password" autoComplete="new-password" value={privacyPasswordConfirmation} onChange={(event) => setPrivacyPasswordConfirmation(event.target.value)} /></label>}
          <button className="primary privacy-save-password" type="submit">{t('settings.privacy.savePassword')}</button>
          </fieldset>
          {privacyStatus && <div className="settings-actions"><span role="status" className={privacyStatus.kind}>{privacyStatus.message}</span></div>}
        </form>
        <section className="ai-settings">
          <label className="setting-row"><span className="ai-settings-toggle-label"><b>{t('settings.ai.enabled')}</b>{aiModelError && <span role="status" className="error ai-model-error">{aiModelError}</span>}</span><input aria-label={t('settings.ai.enabled')} type="checkbox" checked={ai.enabled} onChange={(event) => setAi((current) => ({ ...current, enabled: event.target.checked }))} /></label>
          <fieldset aria-busy={aiBusy}>
            <label><span>{t('settings.ai.serviceAddress')}</span><input aria-label={t('settings.ai.serviceAddress')} required placeholder="https://api.openai.com/v1" value={ai.baseUrl} onChange={(event) => setAi((current) => ({ ...current, baseUrl: event.target.value }))} /></label>
            <label><span>{t('settings.ai.apiKey')}</span><input aria-label={t('settings.ai.apiKey')} type="password" autoComplete="new-password" placeholder={ai.apiKeyConfigured ? t('settings.ai.apiKeyConfigured') : t('settings.ai.apiKeyPlaceholder')} value={aiApiKey} onChange={(event) => setAiApiKey(event.target.value)} /></label>
            <label><span>{t('settings.ai.model')}</span><select ref={aiModelSelect} aria-label={t('settings.ai.model')} required value={ai.model} onPointerDown={() => void reloadAiModels()} onFocus={() => void reloadAiModels()} onKeyDown={(event) => { if (event.key === 'Enter' || event.key === ' ' || event.key === 'ArrowDown') void reloadAiModels(); }} onChange={(event) => void selectAiModel(event.target.value)}><option value="">{t('settings.ai.selectModel')}</option>{ai.model && !aiModels.includes(ai.model) && <option value={ai.model}>{ai.model}</option>}{aiModels.map((model) => <option key={model} value={model}>{model}</option>)}</select></label>
          </fieldset>
          {!onNotice && aiStatus && <div className="settings-actions"><span role="status" className={aiStatus.kind}>{aiStatus.message}</span></div>}
        </section>
      </>}
    </div>
  </section>;
}
