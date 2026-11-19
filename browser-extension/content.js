(() => {
  if (globalThis.VDVideoPickerLoaded) return;
  globalThis.VDVideoPickerLoaded = true;
  const controls = new Map();
  const encryptedPlayers = new WeakSet();
  document.addEventListener('encrypted', event => encryptedPlayers.add(event.target), true);
  document.addEventListener('loadstart', event => encryptedPlayers.delete(event.target), true);
  const t = (key, substitutions) => chrome.i18n.getMessage(key, substitutions);
  let paint = 0;
  const resize = new ResizeObserver(scheduleLayout);
  const isXEmbed = !!VDMedia.xEmbedPostId(location.href);
  const builtIn = globalThis.VDDetection?.isBuiltInPage(location.href) ?? true;
  let enabled = builtIn;

  let closeOpenMenu;

  function playerToken() {
    // Ordinary HTTP pages lack randomUUID(), but can still mint a local token.
    const bytes = crypto.getRandomValues(new Uint8Array(16));
    const hex = Array.from(bytes, byte => byte.toString(16).padStart(2, '0')).join('');
    return [hex.slice(0, 8), hex.slice(8, 12), hex.slice(12, 16), hex.slice(16, 20), hex.slice(20)].join('-');
  }

  function playerIdentity(video, site, sources) {
    if (site?.mediaId) return JSON.stringify([location.href, site.mediaId, site.extractorUrl]);
    return JSON.stringify([location.href, video.currentSrc, video.poster, sources,
      site?.formats?.map(format => [format.kind, format.url, format.audioUrl])]);
  }

  function scheduleLayout() {
    if (!paint) paint = requestAnimationFrame(() => {
      paint = 0;
      for (const control of controls.values()) position(control);
    });
  }

  const visibility = {checkOpacity:true, checkVisibilityCSS:true};
  function thumbnailSurface(image) {
    if (image.checkVisibility?.(visibility) !== false) return image;
    // X's embed uses a transparent <img> for accessibility. A sibling paints
    // that exact poster as a CSS background inside the same attachment link.
    const link = image.closest('a[href]');
    if (!link) return image;
    const source = image.currentSrc || image.src;
    for (const node of [link, ...link.querySelectorAll('[style*="background-image"]')]) {
      const match = /^url\((["']?)(.*?)\1\)$/.exec(getComputedStyle(node).backgroundImage);
      if (match?.[2] === source && node.checkVisibility?.(visibility) !== false) return node;
    }
    return image;
  }

  function position(control) {
    const {video, host} = control;
    if (!video.isConnected) {
      detach(video);
      return;
    }
    const surface = control.thumbnailId ? thumbnailSurface(video) : video;
    const box = surface.getBoundingClientRect();
    const full = document.fullscreenElement;
    const visible = box.width >= 100 && box.height >= 60 && box.bottom > 0 && box.top < innerHeight &&
      box.right >= 50 && box.left < innerWidth && (!full || (full !== video && full.contains(video))) &&
      getComputedStyle(surface).visibility !== 'hidden';
    if (visible && surface.checkVisibility?.(visibility) === false) {
      host.style.display = 'none';
      return;
    }
    host.style.display = visible ? 'block' : 'none';
    if (!visible) return;
    const parent = full || document.documentElement;
    if (host.parentNode !== parent) parent.append(host);
    host.style.left = Math.round(Math.min(innerWidth, box.right) - 8) + 'px';
    host.style.top = Math.round(Math.max(0, box.top) + 8) + 'px';
  }

  function detach(video) {
    const control = controls.get(video);
    if (!control) return;
    control.closeMenu();
    control.lifetime.abort();
    resize.unobserve(video);
    control.host.remove();
    controls.delete(video);
  }

  function embedForCurrentPost(video) {
    const url = VDMedia.xPublicEmbedUrl(location.href);
    if (!url) return null;
    // A status page can also contain replies and quoted posts.
    const owner = video.closest('article');
    const links = [video.closest('a[href]'), ...Array.from(owner?.querySelectorAll('time') || [],
      time => time.closest('a[href]'))];
    const owners = links.map(link => link && VDMedia.xPublicEmbedUrl(link.href)).filter(Boolean);
    return owners.length > 0 && owners.every(link => link === url) ? url : null;
  }

  function attach(video, thumbnailId) {
    if (!enabled || controls.has(video)) return;
    const isVideo = video.localName === 'video';
    // A matched X thumbnail identifies an attachment. Its own src is an image.
    const sourcesForPlayer = () => isVideo ? VDMedia.elementSources(video) : [];
    const host = document.createElement('span');
    Object.assign(host.style, {all:'initial', position:'fixed', display:'none', transform:'translateX(-100%)',
      zIndex:'2147483647', pointerEvents:'none', margin:'0', padding:'0'});
    const shadow = host.attachShadow({mode:'closed'});
    const style = document.createElement('style');
    style.textContent = `
      button { all:initial; display:block; margin-left:auto; cursor:pointer; pointer-events:auto; box-sizing:border-box;
        padding:7px 11px; border:1px solid #f7fff766; border-radius:7px; background:#163b30; color:white;
        font:600 13px/1.35 system-ui,sans-serif; white-space:nowrap; box-shadow:0 1px 5px #0005; }
      button:focus-visible { outline:2px solid white; outline-offset:2px; }
      button:disabled { cursor:wait; }
      output {display:none; max-width:240px; margin:4px 0 0 auto; border-radius:5px; padding:6px 9px;
        color:white; background:#182622; font:12px/1.45 system-ui,sans-serif; }
      output:not(:empty) {display:block;}
      output a {display:block; margin-top:4px; color:#c4f4df; text-decoration:underline; pointer-events:auto;}
      .choices:not([hidden]) { display:grid; gap:3px; box-sizing:border-box; width:max-content; max-width:min(290px,85vw);
        max-height:min(300px,45vh); overflow-y:auto; overscroll-behavior:contain; margin:4px 0 0 auto; padding:4px;
        pointer-events:auto; background:#182622; border:1px solid #6d847b; border-radius:7px; }
      .choices button { width:100%; margin:0; border:0; box-shadow:none; text-align:left; font-weight:500; white-space:normal; }
      .choices button:hover {background:#21624c;}
    `;
    const button = document.createElement('button');
    button.type = 'button';
    button.textContent = t('downloadButton');
    button.title = t('buttonTitle');
    button.setAttribute('aria-expanded', 'false');
    const status = document.createElement('output');
    status.setAttribute('aria-live', 'polite');
    const menu = document.createElement('div');
    menu.className = 'choices';
    menu.id = 'sources';
    menu.hidden = true;
    menu.setAttribute('role', 'group');
    menu.setAttribute('aria-label', t('chooseSource'));
    button.setAttribute('aria-controls', menu.id);
    shadow.append(style, button, menu, status);
    const player = playerToken();
    const lifetime = new AbortController();
    let generation = 0, timer, identity, pinnedMediaId;

    function closeMenu() {
      generation++;
      identity = undefined;
      pinnedMediaId = undefined;
      menu.hidden = true;
      menu.replaceChildren();
      button.setAttribute('aria-expanded', 'false');
      clearTimeout(timer);
      status.textContent = '';
      if (closeOpenMenu === closeMenu) closeOpenMenu = undefined;
    }
    function checkReload() {
      if (pinnedMediaId && globalThis.VDSiteMedia?.inspect(video)?.mediaId === pinnedMediaId) return;
      closeMenu();
    }
    if (isVideo) {
      for (const event of ['loadstart', 'emptied']) video.addEventListener(event, checkReload, {signal:lifetime.signal});
      video.addEventListener('encrypted', closeMenu, {signal:lifetime.signal});
    }
    shadow.addEventListener('keydown', event => {
      if (event.key === 'Escape' && !menu.hidden) { closeMenu(); button.focus(); event.stopPropagation(); }
    });

    async function pick(event, handle) {
      event.stopPropagation();
      if (!event.isTrusted || button.disabled || menu.hidden) return;
      button.disabled = true;
      const version = generation;
      for (const choice of menu.children) choice.disabled = true;
      try {
        const site = await globalThis.VDSiteMedia?.inspect(video);
        if (!video.isConnected || version !== generation || playerIdentity(video, site, sourcesForPlayer()) !== identity) {
          closeMenu(); status.textContent = t('changedMedia'); return;
        }
        if (video.mediaKeys || encryptedPlayers.has(video)) { closeMenu(); status.textContent = t('protectedMedia'); return; }
        status.textContent = t('creating');
        const result = await chrome.runtime.sendMessage({action:'downloadVideo', player, pageUrl:location.href, handle});
        if (generation !== version) return;
        if (result?.ok || ['selectionExpired', 'changedMedia'].includes(result?.reason)) closeMenu();
        status.textContent = t(result?.ok ? 'taskCreated' : result?.reason || 'extensionError');
      } catch { status.textContent = t('extensionError'); }
      finally {
        button.disabled = false;
        for (const choice of menu.children) choice.disabled = false;
        if (video.isConnected && menu.hidden && !lifetime.signal.aborted) {
          timer = setTimeout(() => {status.textContent = '';}, 6500);
        }
      }
    }

    button.addEventListener('click', async event => {
      event.preventDefault();
      event.stopPropagation();
      if (!event.isTrusted || button.disabled || !video.isConnected) return;
      if (!menu.hidden) { closeMenu(); return; }
      closeOpenMenu?.();
      closeMenu();
      closeOpenMenu = closeMenu;
      const version = generation;
      button.disabled = true;
      status.textContent = t('checking');
      try {
        if (video.mediaKeys || encryptedPlayers.has(video)) { status.textContent = t('protectedMedia'); return; }
        const site = await globalThis.VDSiteMedia?.inspect(video);
        if (!isVideo && !(site?.mediaId && site.formats?.length)) { status.textContent = t('changedMedia'); return; }
        const sources = sourcesForPlayer();
        pinnedMediaId = site?.mediaId;
        const snapshot = playerIdentity(video, site, sources);
        const hasMetadata = video.readyState >= HTMLMediaElement.HAVE_METADATA;
        const targets = VDMedia.playbackTargets(location.href, site, sources, document.querySelectorAll('video').length === 1);
        const xCdnMedia = site?.xCdnMedia;
        const xDocumentUrls = xCdnMedia ? [...new Set(performance.getEntriesByType('resource')
          .filter(e => (!e.responseStatus || e.responseStatus < 400) && VDMedia.xAttachmentResource(e.name, xCdnMedia))
          .map(e => e.name))].slice(-32) : undefined;
        const result = targets.length || xCdnMedia
          ? await chrome.runtime.sendMessage({action:'resolveVideo', player, pageUrl:location.href, targets, xCdnMedia, xDocumentUrls})
          : {ok:false, reason:'noVideoTarget'};
        if (version !== generation) return;
        if (!video.isConnected || snapshot !== playerIdentity(video, await globalThis.VDSiteMedia?.inspect(video), sourcesForPlayer())) {
          status.textContent = t('changedMedia'); return;
        }
        const playback = hasMetadata && video.videoWidth && video.videoHeight
          ? t('frameSize', [video.videoWidth + '×' + video.videoHeight]) : '';
        if (!result?.ok) {
          const reason = result?.reason || 'extensionError';
          const notStarted = !hasMetadata && reason === 'noVideoTarget' ? t('playToIdentify') : '';
          status.textContent = [t(reason), notStarted].filter(Boolean).join(' ');
          const embedUrl = ['noVideoTarget','noDownloadFormats'].includes(reason) && embedForCurrentPost(video);
          if (embedUrl) {
            const link = document.createElement('a');
            link.href = embedUrl;
            link.target = '_blank';
            link.rel = 'noopener noreferrer';
            link.textContent = t('openPublicEmbed');
            link.addEventListener('click', e => e.stopPropagation());
            status.append(link);
          }
          return;
        }
        identity = snapshot;
        for (const option of result.options) {
          const choice = document.createElement('button');
          choice.type = 'button';
          choice.textContent = option.label;
          choice.addEventListener('click', e => pick(e, option.handle));
          menu.append(choice);
        }
        menu.hidden = false;
        button.setAttribute('aria-expanded', 'true');
        status.textContent = playback;
      } catch { status.textContent = t('extensionError'); }
      finally { button.disabled = false; }
    });
    controls.set(video, {video, host, closeMenu, lifetime, thumbnailId});
    resize.observe(video);
    position(controls.get(video));
  }

  addEventListener('pagehide', () => closeOpenMenu?.());
  function checkEmbedImage(image) {
    const site = globalThis.VDSiteMedia?.inspect(image);
    const control = controls.get(image);
    if (site?.mediaId && site.formats?.length) {
      if (!control) attach(image, site.mediaId);
      else if (control.thumbnailId !== site.mediaId) {
        control.closeMenu();
        control.thumbnailId = site.mediaId;
      }
      return;
    }
    detach(image);
  }
  if (isXEmbed) {
    let refresh;
    document.addEventListener('vd-embed-media-ready-v1', () => {
      if (!refresh) refresh = requestAnimationFrame(() => {
        refresh = 0;
        for (const img of document.images) checkEmbedImage(img);
      });
    });
    document.addEventListener('load', event => {
      if (event.target.localName === 'img') checkEmbedImage(event.target);
    }, true);
  }
  function scan(node) {
    if (node.nodeType !== Node.ELEMENT_NODE) return;
    if (node.matches('video')) attach(node);
    for (const video of node.querySelectorAll('video')) attach(video);
    if (isXEmbed) {
      if (node.matches('img')) checkEmbedImage(node);
      for (const image of node.querySelectorAll('img')) checkEmbedImage(image);
    }
  }
  if (builtIn) scan(document.documentElement);
  else {
    const setEnabled = value => {
      enabled = value === true;
      if (enabled) scan(document.documentElement);
      else for (const video of controls.keys()) detach(video);
    };
    let changed = false;
    chrome.storage.onChanged.addListener((changes, area) => {
      if (area === 'local' && 'detectAllVideos' in changes) {
        changed = true;
        setEnabled(changes.detectAllVideos.newValue);
      }
    });
    chrome.storage.local.get('detectAllVideos').then(prefs => {
      if (!changed) setEnabled(prefs.detectAllVideos);
    }).catch(() => {});
  }
  new MutationObserver(records => {
    if (!enabled) return;
    for (const record of records) {
      if (record.type === 'attributes' && record.target.localName === 'img') checkEmbedImage(record.target);
      else for (const node of record.addedNodes) scan(node);
    }
    scheduleLayout();
  }).observe(document.documentElement, {childList:true, subtree:true, ...(isXEmbed ? {attributeFilter:['src','srcset']} : {})});
  addEventListener('scroll', scheduleLayout, {capture:true, passive:true});
  addEventListener('resize', scheduleLayout, {passive:true});
  document.addEventListener('fullscreenchange', scheduleLayout);
})();
