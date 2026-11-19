const {setting, webOrigins} = VDDetection;
const permissions = {origins:webOrigins};
const toggle = document.getElementById('detect-all');
const status = document.getElementById('status');
const t = key => chrome.i18n.getMessage(key);

document.documentElement.lang = chrome.i18n.getUILanguage();
document.title = t('addonName');
document.getElementById('label').textContent = t('detectAllVideos');
document.getElementById('help').textContent = t('detectAllHelp');

async function loadSetting() {
  const state = await chrome.storage.local.get(setting);
  toggle.checked = state[setting] === true && await chrome.permissions.contains(permissions);
}

toggle.addEventListener('change', async () => {
  const enabled = toggle.checked;
  toggle.disabled = true;
  status.textContent = '';
  try {
    // Call request directly in the change handler to retain the user gesture.
    if (enabled && !await chrome.permissions.request(permissions)) {
      toggle.checked = false;
      status.textContent = t('detectAllDenied');
      return;
    }
    await chrome.storage.local.set({[setting]:enabled});
    if (!enabled) await chrome.permissions.remove(permissions);
    const result = await chrome.runtime.sendMessage({action:'refreshVideoDetection'});
    if (!result?.ok) throw new Error('Unable to update browser detection');
    toggle.checked = result.enabled;
    if (enabled && toggle.checked) status.textContent = t('detectAllEnabled');
  } catch {
    await loadSetting().catch(() => {});
    status.textContent = t('detectAllError');
  } finally {
    toggle.disabled = false;
  }
});

loadSetting().catch(() => { status.textContent = t('detectAllError'); })
  .finally(() => { toggle.disabled = false; });
