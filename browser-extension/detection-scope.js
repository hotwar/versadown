(() => {
  const setting = 'detectAllVideos';
  const webOrigins = ['http://*/*', 'https://*/*'];
  const builtInMatches = chrome.runtime.getManifest().content_scripts.find(script =>
    script.js.includes('content.js')).matches;
  const builtIn = builtInMatches.map(pattern => {
    const [, scheme, host, path] = /^(https?):\/\/([^/]+)(\/.*)$/.exec(pattern);
    return {scheme, host, path};
  });

  function isBuiltInPage(value) {
    let url;
    try { url = new URL(value); }
    catch { return false; }

    return builtIn.some(({scheme, host, path}) => {
      if (url.protocol !== scheme + ':') return false;
      const subdomains = host.startsWith('*.');
      const domain = subdomains ? host.slice(2) : host;
      if (domain !== url.hostname && !(subdomains && url.hostname.endsWith('.' + domain))) return false;
      return path.endsWith('*') ? url.pathname.startsWith(path.slice(0, -1)) : url.pathname === path;
    });
  }

  globalThis.VDDetection = {
    setting,
    webOrigins,
    builtInMatches,
    isBuiltInPage,
    files: ['detection-scope.js', 'media.js', 'content.js'],
  };
})();
