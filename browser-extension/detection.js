(() => {
  if (!chrome.scripting) return;
  const {setting, webOrigins, builtInMatches, isBuiltInPage, files} = VDDetection;
  const id = 'detect-all-videos';
  let pending = Promise.resolve(false);

  async function reconcile() {
    const {[setting]: enabled} = await chrome.storage.local.get(setting);
    const granted = enabled === true && await chrome.permissions.contains({origins:webOrigins});
    const [existing] = await chrome.scripting.getRegisteredContentScripts({ids:[id]});
    if (granted && !existing) {
      await chrome.scripting.registerContentScripts([{
        id, matches:webOrigins, excludeMatches:builtInMatches, js:files,
        allFrames:true, persistAcrossSessions:true, runAt:'document_idle',
      }]);
    } else if (!granted && existing) {
      await chrome.scripting.unregisterContentScripts({ids:[id]});
    }
    if (enabled === true && !granted) await chrome.storage.local.set({[setting]:false});
    return granted;
  }

  function refresh() {
    // Startup, settings changes and the popup can all arrive together.
    const result = pending.catch(() => false).then(reconcile);
    pending = result;
    return result;
  }

  async function applyToActiveTab() {
    const [tab] = await chrome.tabs.query({active:true, lastFocusedWindow:true});
    if (tab?.id == null || !/^https?:\/\//.test(tab.url ?? '') || isBuiltInPage(tab.url)) return;
    try {
      await chrome.scripting.executeScript({target:{tabId:tab.id, allFrames:true}, files});
    } catch {
      // Some pages restrict injection into a frame. New navigations use the registered script.
    }
  }

  const sync = () => { void refresh().catch(() => {}); };
  chrome.storage.onChanged.addListener((changes, area) => {
    if (area === 'local' && Object.hasOwn(changes, setting)) sync();
  });
  chrome.permissions.onRemoved.addListener(sync);
  chrome.runtime.onStartup.addListener(sync);
  chrome.runtime.onInstalled.addListener(sync);
  chrome.runtime.onMessage.addListener((message, sender, reply) => {
    if (message.action !== 'refreshVideoDetection' || sender.url !== chrome.runtime.getURL('popup.html')) return;
    refresh().then(async enabled => {
      if (enabled) await applyToActiveTab();
      reply({ok:true, enabled});
    }).catch(() => reply({ok:false}));
    return true;
  });
  sync();
})();
