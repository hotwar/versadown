importScripts('media.js');
importScripts('detection-scope.js', 'detection.js');
const host = 'com.versadown.browser';
const manifest = chrome.runtime.getManifest();
const observations = new Map();

// Network observations can annotate an exact player URL.
// A frame, timing, or the newest request cannot identify a player.
const manifestAgeMs = 2 * 60 * 1000;
chrome.webRequest.onHeadersReceived.addListener(details => {
  if (details.tabId < 0 || !details.documentId || details.method !== 'GET' ||
      ![200, 206].includes(details.statusCode) || details.url.length > 8192) return;
  const header = details.responseHeaders?.find(h => h.name.toLowerCase() === 'content-type');
  const type = (header?.value ?? '').split(';')[0].trim().toLowerCase();
  const kind = VDMedia.mediaKind(details.url, type);
  if (!kind) return;
  const doc = details.documentId;
  const items = observations.get(doc) ?? new Map();
  items.delete(details.url);
  items.set(details.url, {type, kind, seen:Date.now()});
  if (items.size > 64) items.delete(items.keys().next().value);
  observations.set(doc, items);
  if (observations.size > 32) observations.delete(observations.keys().next().value);
}, {urls:manifest.host_permissions, types:['media', 'xmlhttprequest', 'other']}, ['responseHeaders']);

async function readManifest(url) {
  const abort = new AbortController();
  const timeout = setTimeout(() => abort.abort(), 2500);
  try {
    const response = await fetch(url, {signal:abort.signal, credentials:'omit', redirect:'error', cache:'no-store'});
    if (!response.ok || !response.body || Number(response.headers.get('content-length')) > 256 * 1024) return '';
    const reader = response.body.getReader();
    const decoder = new TextDecoder();
    let size = 0, body = '';
    while (true) {
      const {value, done} = await reader.read();
      if (done) return body + decoder.decode();
      size += value.length;
      if (size > 256 * 1024) return '';
      body += decoder.decode(value, {stream:true});
      if (body.length > 1024 && !body.trimStart().startsWith('#EXTM3U')) return '';
    }
  } catch {
    return '';
  } finally {
    abort.abort();
    clearTimeout(timeout);
  }
}

// A YouTube player can advertise a progressive itag alongside an active SABR
// session even when ordinary file GETs return an error or an empty body.
async function checkYoutubeFile(url) {
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), 6000);
  try {
    const response = await fetch(url, {headers:{Range:'bytes=0-63'}, credentials:'omit',
      redirect:'error', cache:'no-store', signal:controller.signal});
    if (!response.ok) return {reason:'sourceHttpError', status:response.status};
    const mime = (response.headers.get('content-type') || '').toLowerCase();
    if (!response.body || /^(?:text\/|application\/(?:vnd\.yt-ump|json|xml))/.test(mime)) return {reason:'sourceNotFile'};
    const start = new Uint8Array(12);
    const reader = response.body.getReader();
    let n = 0;
    while (n < start.length) {
      const {done, value} = await reader.read();
      if (done) break;
      const bytes = value.subarray(0, start.length - n);
      start.set(bytes, n);
      n += bytes.length;
    }
    const box = String.fromCharCode(...start.subarray(4,8));
    const mp4 = n >= 12 && (box === 'ftyp' || box === 'moov');
    const webm = n >= 4 && start[0] === 0x1a && start[1] === 0x45 && start[2] === 0xdf && start[3] === 0xa3;
    return mp4 || webm ? null : {reason:'sourceNotFile'};
  } catch {
    return {reason:'sourceUnreachable'};
  } finally {
    controller.abort(); // Also stop a server which ignores Range and sends the entire file.
    clearTimeout(timeout);
  }
}

const menuPrefix = 'vdMenu:';
const menuLifetime = 2 * 60 * 1000;
const sending = new Set();
const videoErrors = new Map([
  ['video.error.drm', 'videoDrm'],
  ['video.error.restricted', 'videoRestricted'],
  ['video.error.cookies', 'videoRestricted'],
  ['video.error.cookieFile', 'videoRestricted'],
  ['video.error.accessDenied', 'videoRestricted'],
  ['video.error.extractor', 'hostUpgrade'],
  ['video.error.rateLimited', 'videoNetwork'],
  ['video.error.network', 'videoNetwork'],
  ['video.error.timeout', 'videoNetwork'],
  ['video.error.previewExpired', 'selectionExpired'],
  ['video.error.formatGone', 'selectionExpired'],
  ['selectionExpired', 'selectionExpired'],
]);

function menuKey(sender, player) {
  return menuPrefix + JSON.stringify([sender.tab.id, sender.documentId, player]);
}

async function saveMenu(key, menu) {
  // Session storage keeps signed URLs in memory through MV3 worker suspension.
  const entries = Object.entries(await chrome.storage.session.get(null)).filter(([k]) => k.startsWith(menuPrefix) && k !== key);
  const expired = entries.filter(([, m]) => m.expires <= Date.now());
  const live = entries.filter(([, m]) => m.expires > Date.now()).sort((a, b) => a[1].expires - b[1].expires);
  const remove = [...expired, ...live.slice(0, Math.max(0, live.length - 7))].map(([k]) => k);
  if (remove.length) await chrome.storage.session.remove(remove);
  await chrome.storage.session.set({[key]:menu});
}

function choiceLabel(format, current) {
  const manifest = format.kind === 'hls' || format.kind === 'dash';
  let label = format.label;
  if (manifest && !format.height && format.evidence !== 'manifest') label = format.kind.toUpperCase() + ' · ' + chrome.i18n.getMessage('automatic');
  else if (format.height && !label.includes(format.height + 'p') && !/\d+×\d+/.test(label)) label = format.height + 'p · ' + label;
  if (current) label += ' · ' + chrome.i18n.getMessage(format.kind === 'pair' ? 'currentVideoTrack' : 'currentSource');
  return label;
}

function xNetworkCandidates(observed, media, pageUrl, documentUrls) {
  if (!/^(ext_tw_video|amplify_video)\/\d{1,24}$/.test(media) ||
      !/(^|\.)x\.com$/.test(new URL(pageUrl).hostname)) return [];
  const urls = new Set(Array.isArray(documentUrls) ? documentUrls.slice(0,32) : []);
  for (const [url, item] of observed ?? []) if (Date.now() - item.seen <= manifestAgeMs) urls.add(url);
  const found = [...urls].map(url => VDMedia.xAttachmentResource(url, media)).filter(Boolean);
  return found.sort((a,b) => (b.height || 0) - (a.height || 0));
}

async function discover(message, sender, key) {
  const pageUrl = message.pageUrl;
  const observed = observations.get(sender.documentId);
  const candidates = Array.isArray(message.formats) ? message.formats.slice(0, 48) : [];
  const currentTracks = new Set(candidates.filter(f => f?.current && f.kind === 'pair').map(f => f.url));
  const currentUrls = new Set();
  for (const source of (message.sources ?? []).slice(0, 32)) {
    const url = VDMedia.resourceUrl(source.url, pageUrl);
    if (!url) continue;
    if (source.current) currentUrls.add(url);
    const mime = observed?.get(url)?.type || source.type || '';
    const kind = VDMedia.mediaKind(url, mime);
    if (!kind) continue;
    const sized = source.current && kind === 'direct' && Number.isInteger(message.width) && message.width > 0 && message.width <= 16384 && Number.isInteger(message.height) && message.height > 0 && message.height <= 16384;
    const label = (sized ? message.width + '×' + message.height + ' · ' : '') + (mime || kind.toUpperCase());
    candidates.push({url, kind, label, height:source.current && kind === 'direct' ? message.height : undefined,
      evidence:observed?.has(url) ? 'observed' : 'player', mimeType:mime});
  }
  const declared = new Set(candidates.map(f => f?.url));
  const network = xNetworkCandidates(observed, message.xCdnMedia, pageUrl, message.xDocumentUrls);
  const inferredMasters = new Set(network.filter(f => f.kind === 'hls' && !declared.has(f.url)).map(f => f.url));
  candidates.push(...network);
  let formats = VDMedia.collectFormats(candidates, pageUrl);
  const hls = formats.filter(f => f.kind === 'hls' && !f.height).slice(0, 3);
  const expansions = await Promise.all(hls.map(async f => ({format:f, body:await readManifest(f.url)})));
  formats = formats.filter(f => !inferredMasters.has(f.url) || expansions.some(e =>
    e.format === f && /^#EXTM3U\s/.test(e.body.trimStart()) && /^#EXT-X-STREAM-INF:/m.test(e.body)));
  for (const {format, body} of expansions) {
    if (/^#EXT-X-(?:SESSION-)?KEY:.*\b(?:METHOD=SAMPLE-AES|KEYFORMAT="(?!identity"))/im.test(body)) {
      formats = formats.filter(f => f !== format);
      continue;
    }
    formats.push(...VDMedia.hlsVariants(body, format.url, pageUrl));
  }
  formats = VDMedia.collectFormats(formats, pageUrl);
  const reported = Array.isArray(message.reportedHeights) ? message.reportedHeights : [];
  const heights = [...new Set(reported.slice(0, 128).filter(height =>
    Number.isInteger(height) && height > 0 && height <= 16384))].sort((a,b) => a-b).slice(0, 32);
  const selectable = new Set(formats.map(format => format.height));
  const reportedOnly = heights.filter(height => !selectable.has(height));
  if (!formats.length) {
    await chrome.storage.session.remove(key);
    const unboundManifest = [...(observed?.values() ?? [])].some(item =>
      (item.kind === 'hls' || item.kind === 'dash') && Date.now() - item.seen < manifestAgeMs);
    return {ok:false, reason:reportedOnly.length ? 'reportedOnly' : unboundManifest ? 'unboundManifest' : 'noMedia', reportedOnly};
  }
  const choices = formats.map(format => ({handle:crypto.randomUUID(), format}));
  const title = VDMedia.textLabel(message.title, new URL(pageUrl).hostname, 240);
  await saveMenu(key, {pageUrl, title, expires:Date.now() + menuLifetime, choices});
  const options = choices.map(({handle, format}) => {
    const current = (format.kind === 'direct' && currentUrls.has(format.url)) ||
      (format.kind === 'pair' && currentTracks.has(format.url));
    return {handle, label:choiceLabel(format, current)};
  });
  const labels = options.map(option => option.label);
  for (const [index, option] of options.entries()) {
    if (labels.indexOf(option.label) !== labels.lastIndexOf(option.label)) option.label = chrome.i18n.getMessage('numberedOption', [String(index + 1), option.label]);
  }
  return {ok:true, options, reportedOnly};
}

async function createTask(message, sender, key) {
  if (sending.has(key)) return {ok:false, reason:'selectionExpired'};
  sending.add(key);
  try {
    const menu = (await chrome.storage.session.get(key))[key];
    const choice = menu?.choices.find(row => row.handle === message.handle);
    if (!choice || menu.pageUrl !== message.pageUrl || menu.expires <= Date.now()) return {ok:false, reason:'selectionExpired'};
    if (!choice.checked && /(^|\.)youtube\.com$/.test(new URL(menu.pageUrl).hostname) &&
        ['pair','direct'].includes(choice.format.kind)) {
      const urls = [choice.format.url, choice.format.audioUrl].filter(Boolean);
      const checks = await Promise.all(urls.map(checkYoutubeFile));
      const failure = checks.find(Boolean);
      if (failure) return {ok:false, ...failure};
      // Preserve retries after a lost native acknowledgment with the same ID,
      // even if the signed URL expires after the first successful transfer.
      choice.checked = true;
      await chrome.storage.session.set({[key]:menu});
    }
    // The stable request ID also covers a retry after a lost native reply.
    const request = {version:2, requestId:choice.handle, pageUrl:menu.pageUrl, title:menu.title, formats:[choice.format]};
    let result;
    try { result = await chrome.runtime.sendNativeMessage(host, request); }
    catch { return {ok:false, reason:'hostUnavailable'}; }
    if (result?.ok !== true) return {ok:false, reason:'hostRejected'};
    await chrome.storage.session.remove(key);
    return {ok:true};
  } finally { sending.delete(key); }
}

function nativeFailure(result, fallback) {
  if (!result || typeof result.ok !== 'boolean' || (!result.reason && result.error === 'handoff_failed')) {
    return {ok:false, reason:'hostUpgrade'};
  }
  return {ok:false, reason:videoErrors.get(result.reason) || fallback};
}

async function* playbackUrls(message, sender) {
  const seen = new Set();
  const targets = Array.isArray(message.targets) ? message.targets.slice(0, 2) : [];
  for (const target of targets) {
    if (!VDMedia.canResolveUrl(target?.url, message.pageUrl, target?.kind) || seen.has(target.url)) continue;
    seen.add(target.url);
    yield target.url;
  }
  // X can reveal an attachment's CDN identity before its playback description.
  // Prefer a master playlist for that exact attachment when no page result exists.
  const xCandidates = xNetworkCandidates(observations.get(sender.documentId), message.xCdnMedia, message.pageUrl, message.xDocumentUrls);
  for (const {url} of xCandidates.filter(candidate => candidate.kind === 'hls' && !seen.has(candidate.url)).slice(0, 3)) {
    const body = (await readManifest(url)).trimStart();
    if (body.startsWith('#EXTM3U') && /^#EXT-X-STREAM-INF:/m.test(body)) {
      yield url;
      return;
    }
  }
}

async function resolveVideo(message, sender, key) {
  await chrome.storage.session.remove(key);
  let failure = {ok:false, reason:'noVideoTarget'};
  for await (const pageUrl of playbackUrls(message, sender)) {
    let result;
    try {
      result = await chrome.runtime.sendNativeMessage(host, {
        version:3, action:'resolve', requestId:crypto.randomUUID(), pageUrl,
      });
    } catch {
      return {ok:false, reason:'hostUnavailable'};
    }
    if (result?.ok !== true) {
      failure = nativeFailure(result, 'noDownloadFormats');
      if (failure.reason === 'videoDrm' || failure.reason === 'hostUpgrade') return failure;
      continue;
    }
    if (typeof result.previewId !== 'string' || !/^[a-f0-9-]{36}$/i.test(result.previewId) ||
        !Array.isArray(result.choices)) return {ok:false, reason:'hostUpgrade'};
    const choices = result.choices.slice(0, 64).flatMap(format => {
      if (!format || typeof format.id !== 'string' || !format.id || format.id.length > 512) return [];
      const size = format.width > 0 && format.height > format.width
        ? format.width + '×' + format.height : format.height > 0 ? format.height + 'p' : '';
      const quality = VDMedia.textLabel(format.label, size || chrome.i18n.getMessage('videoUnknownQuality'), 135);
      const label = [quality, format.ext && quality !== format.ext ? String(format.ext).toUpperCase() : ''].filter(Boolean).join(' · ');
      return [{handle:crypto.randomUUID(), formatId:format.id, label}];
    });
    if (!choices.length) {
      failure = {ok:false, reason:'noDownloadFormats'};
      continue;
    }
    await saveMenu(key, {pageUrl:message.pageUrl, previewId:result.previewId, expires:Date.now() + menuLifetime, choices});
    return {ok:true, options:choices.map(({handle, label}) => ({handle, label}))};
  }
  return failure;
}

async function downloadVideo(message, key) {
  if (sending.has(key)) return {ok:false, reason:'selectionExpired'};
  sending.add(key);
  try {
    const menu = (await chrome.storage.session.get(key))[key];
    const choice = menu?.choices.find(row => row.handle === message.handle);
    if (!menu?.previewId || !choice || menu.pageUrl !== message.pageUrl || menu.expires <= Date.now()) {
      return {ok:false, reason:'selectionExpired'};
    }
    let result;
    try {
      result = await chrome.runtime.sendNativeMessage(host, {
        version:3, action:'select', requestId:choice.handle, previewId:menu.previewId, formatId:choice.formatId,
      });
    } catch {
      return {ok:false, reason:'hostUnavailable'};
    }
    if (result?.ok !== true) return nativeFailure(result, 'hostRejected');
    await chrome.storage.session.remove(key);
    return {ok:true};
  } finally {
    sending.delete(key);
  }
}

chrome.runtime.onMessage.addListener((message, sender, reply) => {
  const plus = ['resolveVideo', 'downloadVideo'].includes(message?.action);
  if ((!plus && !['discover', 'createTask'].includes(message?.action)) || sender.id !== chrome.runtime.id || sender.tab?.id == null ||
      !sender.documentId || typeof message.player !== 'string' || !/^[a-f0-9-]{36}$/i.test(message.player) ||
      !(plus ? VDMedia.webUrl(sender.url) : VDMedia.resourceUrl(sender.url, sender.url))) return;
  // Chrome can keep sender.url at the initial document URL after pushState.
  // The isolated content script supplies the current same-origin frame URL;
  // its documentId, player and route are pinned to the subsequent choice.
  const currentPage = plus ? VDMedia.webUrl(message.pageUrl)?.href : VDMedia.resourceUrl(message.pageUrl, sender.url);
  if (!currentPage || new URL(currentPage).origin !== new URL(sender.url).origin) {
    reply({ok:false, reason:'changedMedia'}); return;
  }
  const key = menuKey(sender, message.player);
  async function handle() {
    if (!plus) return message.action === 'discover' ? discover(message, sender, key) : createTask(message, sender, key);
    if (globalThis.VDDetection && !VDDetection.isBuiltInPage(currentPage)) {
      const prefs = await chrome.storage.local.get(VDDetection.setting);
      if (prefs[VDDetection.setting] !== true) return {ok:false, reason:'noVideoTarget'};
    }
    const scoped = {...message, pageUrl:currentPage};
    return message.action === 'resolveVideo' ? resolveVideo(scoped, sender, key) : downloadVideo(scoped, key);
  }
  const task = handle();
  task.then(reply, () => reply({ok:false, reason:'extensionError'}));
  return true;
});
