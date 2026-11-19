// Page-world adapter. Read only playback descriptions already returned to the
// page. The isolated content script owns the click and the desktop handoff.
(() => {
  'use strict';
  const host = location.hostname;
  const under = (s, domain) => s === domain || s.endsWith('.' + domain);
  const bili = under(host, 'bilibili.com');
  const yt = under(host, 'youtube.com');
  const x = under(host, 'x.com');
  function embeddedPostId() {
    if (window !== window.top || location.origin !== 'https://platform.twitter.com' || location.pathname !== '/embed/Tweet.html') return;
    const ids = new URL(location.href).searchParams.getAll('id');
    if (ids.length === 1 && /^\d{1,20}$/.test(ids[0])) return ids[0];
  }
  const embedId = embeddedPostId();
  const sharedEmbedPosters = new Set();
  if (!bili && !yt && !x && !embedId) return;
  const bootUrl = location.href;
  const cache = new Map();
  const element = (value) => value instanceof HTMLVideoElement;
  const list = (value) => Array.isArray(value) ? value : [];
  function remember(id, record) {
    if (!id || !(record.formats?.length || record.reportedHeights?.length)) return;
    cache.delete(id);
    cache.set(id, record);
    if (cache.size > 80) cache.delete(cache.keys().next().value);
  }
  function mediaUrl(value) {
    if (typeof value !== 'string' || value.length > 16384) return undefined;
    try {
      const u = new URL(value, location.href);
      if (u.protocol !== 'https:' || u.username || u.password) return undefined;
      const domains = yt ? ['googlevideo.com'] : x ? ['video.twimg.com'] :
        ['bilibili.com', 'bilivideo.com', 'bilivideo.cn', 'hdslb.com'];
      return domains.some((d) => under(u.hostname, d)) ? u.href : undefined;
    } catch { return undefined; }
  }
  const format = (id, kind, url, height, audioUrl) => ({
    id: String(id), kind, url, ...(audioUrl ? {audioUrl} : {}),
    ...(Number.isInteger(height) && height > 0 ? {height} : {}),
    label: Number.isInteger(height) && height > 0 ? height + 'p' : kind.toUpperCase(),
    evidence: 'player'
  });
  function describeTrack(candidate, row) {
    const codecs = row.codecs || row.mimeType || row.mime_type || '';
    const codec = /avc1|avc3/i.test(codecs) ? 'H.264' :
      /av01/i.test(codecs) ? 'AV1' : /hev1|hvc1/i.test(codecs) ? 'HEVC' :
      /vp09|vp9/i.test(codecs) ? 'VP9' : '';
    const fps = Number(row.fps ?? row.frameRate ?? row.frame_rate);
    candidate.label = [candidate.label, codec, fps > 30 && fps <= 240 ? Math.round(fps) + ' fps' : '']
      .filter(Boolean).join(' · ');
    return candidate;
  }
  function clearYT(row) {
    return !row.signatureCipher && !row.cipher && !row.drmFamilies?.length && !row.licenseInfos?.length;
  }
  function byHeight(a, b) {
    return (b.height || 0) - (a.height || 0);
  }
  function youtubeUnavailable(data) {
    return (data.playabilityStatus?.status && data.playabilityStatus.status !== 'OK') ||
      data.videoDetails?.isLive || (data.videoDetails?.isLiveContent && data.playabilityStatus?.liveStreamability) ||
      data.streamingData?.licenseInfos?.length || data.streamingData?.drmFamilies?.length;
  }
  function youtubeRecord(data) {
    const id = data?.videoDetails?.videoId;
    const info = data?.streamingData;
    if (!id || !info || youtubeUnavailable(data)) return;
    const adapt = list(info.adaptiveFormats).filter(clearYT);
    const audio = adapt.filter((r) => /^audio\/mp4/.test(r.mimeType) && mediaUrl(r.url))
      .sort((a, b) => Number(!!b.audioTrack?.audioIsDefault) - Number(!!a.audioTrack?.audioIsDefault) ||
        (b.bitrate || 0) - (a.bitrate || 0))[0];
    const formats = [];
    for (const row of list(info.formats).filter(clearYT)) {
      const url = mediaUrl(row.url);
      if (url && /^video\//.test(row.mimeType) && (row.audioQuality || row.audioChannels || /mp4a|opus|vorbis/i.test(row.mimeType))) {
        formats.push(describeTrack(format('yt-' + row.itag, 'direct', url, row.height), row));
      }
    }
    if (audio) for (const row of adapt) {
      const url = mediaUrl(row.url);
      if (url && /^video\/mp4/.test(row.mimeType)) {
        formats.push(describeTrack(format('yt-' + row.itag, 'pair', url, row.height, mediaUrl(audio.url)), row));
      }
    }
    for (const [field, kind] of [['hlsManifestUrl', 'hls'], ['dashManifestUrl', 'dash']]) {
      const url = mediaUrl(info[field]);
      if (url) formats.push(format('yt-' + kind, kind, url));
    }
    const rows = [...list(info.formats), ...list(info.adaptiveFormats)];
    const reportedHeights = [...new Set(rows
      .map(row => row.height).filter(h => Number.isInteger(h) && h > 0 && h <= 16384))].sort((a, b) => b - a);
    // A progressive fallback can coexist with a SABR session for adaptive
    // playback. The caller can report the session when ordinary URLs fail.
    const session = mediaUrl(info.serverAbrStreamingUrl) && !info.hlsManifestUrl && !info.dashManifestUrl;
    return {id, mediaId:'yt:' + id, title:data.videoDetails.title, formats:formats.sort(byHeight), reportedHeights,
      ...(session ? {playbackTransport:'sabr'} : {})};
  }
  function biliRecord(body) {
    const data = body?.data || body?.result || body;
    const dash = data?.dash;
    if (!data || data.is_drm || data.drm_tech_type > 0) return;
    // An ISP mirror can use an unsupported port or contain a single range.
    // Pick only complete URLs declared for this particular representation.
    const trackUrls = row => [row.baseUrl || row.base_url || row.url, ...list(row.backupUrl || row.backup_url)]
      .filter(raw => typeof raw === 'string' && raw.length <= 8192 && /^(?:https:)?\/\//i.test(raw))
      .map(mediaUrl).filter(url => {
        if (!url) return false;
        const u = new URL(url);
        return !u.port && !/\.(?:ts|cmfa|cmfv)$/i.test(u.pathname) &&
          !['range','sq','segment','sequence'].some(k => u.searchParams.has(k));
      });
    const audioUrl = list(dash?.audio).filter(f => !f.drm && /^audio\/mp4/.test(f.mimeType || f.mime_type))
      .sort((a, b) => b.bandwidth - a.bandwidth).map(f => trackUrls(f)[0]).find(Boolean);
    const formats = [];
    const alternatives = new Map();
    if (audioUrl) for (const row of list(dash.video)) {
      const [url, ...backups] = trackUrls(row);
      if (url && !row.drm && /^video\/mp4/.test(row.mimeType || row.mime_type)) {
        formats.push(describeTrack(format('b-' + row.id + '-' + (row.codecid || row.codecs || 'v'), 'pair', url, row.height, audioUrl), row));
        alternatives.set(url, backups);
      }
    }
    const files = list(data.durl);
    if (files.length === 1 && /mp4/i.test(data.format || '')) {
      const [url] = trackUrls(files[0]);
      if (url) formats.push(format('b-' + data.quality, 'direct', url));
    }
    const heights = list(dash?.video).map(row => row.height);
    for (const row of list(data.support_formats)) {
      const label = [row.new_description, row.display_desc, row.format].join(' ');
      const explicit = label.match(/(?:^|[^0-9])(\d{3,4})p(?:[^0-9]|$)/i);
      if (explicit) heights.push(Number(explicit[1]));
    }
    const reportedHeights = [...new Set(heights.filter(h => Number.isInteger(h) && h > 0 && h <= 16384))];
    return {formats: formats.sort(byHeight), reportedHeights, alternatives};
  }
  function inspectBili(video) {
    // Bilibili exposes the actual HTML media element and the current part CID.
    // This also works in its official external player, which has no watch-page state.
    const api = window.player;
    const active = typeof api?.mediaElement === 'function' ? api.mediaElement() : api?.mediaElement;
    const ownPlayer = active === video;
    const liveCid = ownPlayer ? api.getManifest?.()?.cid : undefined;
    const owner = video.closest('[data-cid]');
    let cid = liveCid || (owner?.querySelectorAll('video').length === 1 && owner.dataset.cid);
    let record = cid ? cache.get(String(cid)) : null;
    if (!record) {
      const watch = video.closest('#bilibili-player');
      if (!ownPlayer && (element(active) || !watch || watch.querySelectorAll('video').length !== 1)) return;
      const state = window.__INITIAL_STATE__?.videoData;
      const url = new URL(location.href), boot = new URL(bootUrl);
      const bvid = url.pathname.split('/')[2]?.toLowerCase();
      if (!state?.bvid || bvid !== String(state.bvid).toLowerCase()) return;
      const part = Number(url.searchParams.get('p') || 1);
      const partCid = state.pages?.find(p => Number(p.page) === part)?.cid || (part === 1 && state.cid);
      if (!partCid || (cid && String(partCid) !== String(cid))) return;
      cid = partCid;
      record = cache.get(String(cid));
      // Tracking parameters, fragments and playback timestamps don't select a different part.
      if (!record && boot.pathname.split('/')[2]?.toLowerCase() === bvid &&
          Number(boot.searchParams.get('p') || 1) === part) record = biliRecord(window.__playinfo__);
    }
    const playback = new URL(location.href);
    const state = window.__INITIAL_STATE__?.videoData;
    const watchId = /^\/video\/(BV[\w]+|av\d+)\/?$/i.exec(playback.pathname)?.[1];
    const embedId = playback.hostname === 'player.bilibili.com' && playback.pathname === '/player.html' &&
      playback.searchParams.get('bvid');
    const part = Number(playback.searchParams.get(watchId ? 'p' : 'page') || 1);
    const stateCid = state?.pages?.find(p => Number(p.page) === part)?.cid || (part === 1 && state?.cid);
    const boot = new URL(bootUrl);
    const onBootPart = boot.pathname.replace(/\/$/, '') === playback.pathname.replace(/\/$/, '') &&
      Number(boot.searchParams.get('p') || 1) === part;
    const id = (watchId && String(stateCid) === String(cid) &&
      (record || liveCid || onBootPart) &&
      (watchId.toLowerCase() === state?.bvid?.toLowerCase() || watchId.toLowerCase() === 'av' + state?.aid) && watchId) ||
      (embedId && ownPlayer && /^BV[\w]+$/i.test(embedId) &&
        (playback.searchParams.has('cid') ? playback.searchParams.get('cid') === String(cid) : !!record) && embedId);
    const extractorUrl = id && Number.isSafeInteger(part) && part > 0
      ? 'https://www.bilibili.com/video/' + id + '/' + (part === 1 ? '' : '?p=' + part) : undefined;
    if (!record) return extractorUrl ? {mediaId:'bili:' + cid, extractorUrl, formats:[]} : undefined;
    let playUrl;
    try { if (ownPlayer) playUrl = api.getMediaInfo?.()?.playUrl; }
    catch { /* A player changing quality may temporarily have no active track. */ }
    // getMediaInfo joins the active signed video and audio URLs with a comma.
    // A comma *inside* a signed URL is meaningful (e.g. qn_dyeid=0,0).
    const urls = typeof playUrl === 'string' ? playUrl.split(/,(?=https:\/\/)/).map(mediaUrl) : [];
    const current = record.formats.filter(f => urls.some(u => f.url === u || record.alternatives.get(f.url)?.includes(u)));
    const formats = current.length === 1
      ? record.formats.map(f => f === current[0] ? {...f, current:true} : f) : record.formats;
    return {mediaId:'bili:' + cid, extractorUrl, formats, reportedHeights:record.reportedHeights};
  }
  function xCdnUrl(value, hostname) {
    if (typeof value !== 'string' || !value || value.length > 8192) return;
    try {
      const url = new URL(value);
      if (url.protocol === 'https:' && url.hostname === hostname && !url.port && !url.username && !url.password) return url;
    } catch { /* A blob or an empty poster has no CDN identity. */ }
  }
  function xCdnKey(url) {
    const match = url?.pathname.match(/^\/(amplify_video|ext_tw_video)(?:_thumb)?\/(\d{1,20})\//);
    return match && match[1] + '/' + match[2];
  }
  function xPosterKey(url) {
    if (!url || url.pathname === '/') return;
    // X may request another size or encoding of the same image.
    const key = new URL(url.href);
    key.searchParams.delete('name');
    key.searchParams.delete('format');
    key.hash = '';
    return key.href;
  }
  function xAttachmentLink(value) {
    if (typeof value !== 'string' || value.length > 8192) return;
    try {
      const url = new URL(value);
      if (url.protocol !== 'https:' || url.port || url.username || url.password ||
          !['x.com', 'www.x.com', 'twitter.com', 'www.twitter.com'].includes(url.hostname)) return;
      const match = /^\/[\w]+\/status\/(\d{1,20})\/video\/([1-4])\/?$/.exec(url.pathname);
      return match && match[1] + '/video/' + match[2];
    } catch { /* Only canonical attachment links can identify this video. */ }
  }
  function xExtractorUrl(attachment) {
    return attachment ? 'https://x.com/i/web/status/' + attachment : undefined;
  }
  function xFormats(variants) {
    const formats = [];
    for (const variant of list(variants)) {
      const variantUrl = xCdnUrl(variant?.url, 'video.twimg.com');
      if (!variantUrl) continue;
      const url = variantUrl.href;
      const mime = String(variant.content_type || '').split(';')[0].trim().toLowerCase();
      const id = 'x-' + formats.length;
      if (mime.includes('mpegurl')) {
        formats.push(format(id, 'hls', url));
      } else if (mime === 'video/mp4') {
        // Only this rendition's URL describes its size. The attachment's
        // original dimensions apply to the whole video, including lower rungs.
        const size = variantUrl.pathname.match(/\/(\d{2,5})x(\d{2,5})\//);
        const width = Number(size?.[1]), height = Number(size?.[2]);
        const hasSize = width > 0 && height > 0 && width <= 16384 && height <= 16384;
        const row = format(id, 'direct', url, hasSize ? height : undefined);
        if (hasSize) {
          row.width = width;
          row.label = width + '×' + height;
        }
        const bitrate = Number(variant.bitrate ?? variant.bit_rate);
        if (Number.isSafeInteger(bitrate) && bitrate > 0) {
          row.bitrate = bitrate;
          const rate = Math.max(1, Math.round(bitrate / 1000)) + ' kbps';
          row.label = hasSize ? row.label + ' · ' + rate : rate;
        }
        formats.push(row);
      }
    }
    const area = row => (row.width || 0) * (row.height || 0);
    return formats.sort((a, b) => area(b) - area(a) || (b.bitrate || 0) - (a.bitrate || 0) ||
      Number(b.kind === 'hls') - Number(a.kind === 'hls'));
  }
  let xRecordNumber = 0;
  function rememberXMedia(node, variants, title) {
    const formats = xFormats(variants);
    if (!formats.length) return;
    const sourceKeys = new Set(formats.map(row => xCdnKey(new URL(row.url))).filter(Boolean));
    if (sourceKeys.size > 1) return;
    const sourceKey = [...sourceKeys][0];
    const declaredId = (typeof node.media_key === 'string' && node.media_key.match(/^\d+_(\d+)$/)?.[1]) ||
      (typeof node.id_str === 'string' && /^\d+$/.test(node.id_str) ? node.id_str : undefined);
    const posters = [node.media_url_https, node.preview_image_url]
      .map(value => xCdnUrl(value, 'pbs.twimg.com')).filter(Boolean);
    const posterMediaKeys = new Set(posters.map(xCdnKey).filter(Boolean));
    const posterKeys = new Set(posters.map(xPosterKey).filter(Boolean));
    const key = sourceKey ? 'cdn:' + sourceKey : declaredId ? 'media:' + declaredId :
      'urls:' + JSON.stringify(formats.map(row => row.url).sort());
    const previous = cache.get(key);
    const mediaId = previous?.mediaId || (declaredId || sourceKey ? key : 'item:' + ++xRecordNumber);
    const attachmentLink = xAttachmentLink(node.expanded_url) || previous?.attachmentLink;
    remember(key, {mediaId, declaredId, sourceKey, posterMediaKeys, posterKeys, formats, attachmentLink, title});
  }
  function rememberX(data) {
    const nodes = [data];
    for (let i = 0; i < 20000 && nodes.length; i++) {
      const node = nodes.pop();
      if (!node || typeof node !== 'object') continue;
      if (Array.isArray(node.video_info?.variants)) {
        rememberXMedia(node, node.video_info.variants);
      } else if (['video', 'animated_gif'].includes(node.type) && Array.isArray(node.variants)) {
        rememberXMedia(node, node.variants);
      } else {
        for (const child of Object.values(node)) if (child && typeof child === 'object') nodes.push(child);
      }
    }
  }
  function rememberXEmbed(data) {
    if (!data || data.id_str !== embedId || embeddedPostId() !== embedId) return;
    cache.clear();
    sharedEmbedPosters.clear();
    const seen = new Set();
    // mediaDetails belongs to the outer post. Quoted posts and link cards
    // can include their own video_info and must not enter this collection.
    for (const media of list(data.mediaDetails)) {
      const poster = xPosterKey(xCdnUrl(media?.media_url_https, 'pbs.twimg.com'));
      if (poster && seen.has(poster)) sharedEmbedPosters.add(poster);
      seen.add(poster);
      if (!['video','animated_gif'].includes(media?.type)) continue;
      const attachment = xAttachmentLink(media.expanded_url);
      if (!attachment?.startsWith(embedId + '/video/')) continue;
      const author = String(data.user?.screen_name || 'X').slice(0, 32);
      const title = author + ' ' + embedId + ' - ' + attachment.split('/').pop();
      rememberXMedia(media, media.video_info?.variants, title);
    }
    document.dispatchEvent(new Event('vd-embed-media-ready-v1'));
  }
  function inspectXEmbed(image) {
    if (embeddedPostId() !== embedId || !image.isConnected) return;
    const url = xCdnUrl(image.currentSrc || image.src, 'pbs.twimg.com');
    if (!url) return;
    const anchor = image.closest('a[href]');
    if (!anchor || !['https://x.com','https://twitter.com','https://www.x.com','https://www.twitter.com'].includes(anchor.origin)) return;
    const link = /^\/[\w]+\/status\/(\d{1,20})(?:\/video\/([1-4]))?\/?$/.exec(anchor.pathname);
    if (link?.[1] !== embedId) return;
    const key = xPosterKey(url);
    if (!key || sharedEmbedPosters.has(key)) return;
    const matches = [...cache.values()].filter(rec => rec.posterKeys.has(key));
    if (matches.length !== 1 || (link[2] && matches[0].attachmentLink !== xAttachmentLink(anchor.href))) return;
    const {mediaId, formats, title, attachmentLink} = matches[0];
    return {mediaId, formats, title, extractorUrl:xExtractorUrl(attachmentLink)};
  }
  function readResponse(rawUrl, data) {
    const url = new URL(rawUrl, location.href);
    if (bili) {
      const record = biliRecord(data);
      if (record) remember(url.searchParams.get('cid'), record);
    } else if (yt) {
      const record = youtubeRecord(data);
      if (record) remember(record.id, record);
    } else if (embedId) rememberXEmbed(data);
    else rememberX(data);
  }
  function isPlaybackResponse(rawUrl) {
    if (!rawUrl) return false;
    try {
      const u = new URL(rawUrl, location.href);
      if (embedId) return u.origin === 'https://cdn.syndication.twimg.com' && !u.username && !u.password &&
        u.pathname === '/tweet-result' && u.searchParams.getAll('id').length === 1 && u.searchParams.get('id') === embedId;
      return bili ? u.hostname === 'api.bilibili.com' && /^\/x\/player\/(?:wbi\/)?playurl$/.test(u.pathname)
        : yt ? under(u.hostname, 'youtube.com') && u.pathname === '/youtubei/v1/player'
        : under(u.hostname, 'x.com') && /^\/(?:i\/api\/|api\/)?graphql\//.test(u.pathname);
    } catch { return false; }
  }
  const limit = 2 * 1024 * 1024;
  async function readFetch(response) {
    if (!isPlaybackResponse(response.url) || !response.ok ||
        !/json/i.test(response.headers.get('content-type') || '') || Number(response.headers.get('content-length')) > limit) return;
    const reader = response.clone().body?.getReader();
    if (!reader) return;
    const decoder = new TextDecoder();
    let size = 0, text = '';
    try {
      for (;;) {
        const {value, done} = await reader.read();
        if (done) break;
        size += value.length;
        if (size > limit) { void reader.cancel().catch(() => {}); return; }
        text += decoder.decode(value, {stream: true});
      }
      readResponse(response.url, JSON.parse(text + decoder.decode()));
    } finally { reader.releaseLock(); }
  }
  const fetch = window.fetch;
  window.fetch = function (...args) {
    const promise = Reflect.apply(fetch, this, args);
    promise.then(r => readFetch(r)).catch(() => {});
    return promise;
  };
  const send = XMLHttpRequest.prototype.send;
  XMLHttpRequest.prototype.send = function (...args) {
    this.addEventListener('load', function () {
      try {
        if (!isPlaybackResponse(this.responseURL) || this.status !== 200) return;
        if (Number(this.getResponseHeader('content-length')) > limit) return;
        const body = this.responseType === 'json' ? this.response :
          (!this.responseType || this.responseType === 'text') && this.responseText.length <= limit ? JSON.parse(this.responseText) : null;
        if (body) readResponse(this.responseURL, body);
      } catch { /* Non-JSON response, or the player discarded its request. */ }
    }, {once: true});
    return Reflect.apply(send, this, args);
  };
  function xComponentPosters(video, records) {
    const playerSelector = '[data-testid="videoComponent"],[data-testid="videoPlayer"]';
    // X can paint the poster inside the video component instead of on <video>.
    // Restrict sibling evidence to X's video thumbnails. The post's other
    // attachments, avatars and link previews cannot identify this player.
    const posters = [];
    for (let player = video.closest(playerSelector); player; player = player.parentElement?.closest(playerSelector)) {
      if (player.querySelectorAll('video').length !== 1) break;
      for (const image of player.querySelectorAll('img, [style*="background-image"]')) {
        if (image.closest(playerSelector) !== player) continue;
        const background = image.style.backgroundImage.match(/^url\(["']?([^"')]+)["']?\)$/)?.[1];
        const url = xCdnUrl(image.currentSrc || image.src || background, 'pbs.twimg.com');
        if (!url) continue;
        // Opaque /media/ names have no attachment ID. Accept only an exact
        // thumbnail returned in the site's description of a video.
        if (xCdnKey(url) || (url.pathname.startsWith('/media/') &&
            records.some(rec => rec.posterKeys.has(xPosterKey(url))))) posters.push(url);
      }
    }
    return posters;
  }
  function inspectX(video) {
    if (video.srcObject || video.closest('[data-testid="placementTracking"]') ||
        video.closest('article')?.querySelector('[data-testid="promotedIndicator"]')) return;
    const current = video.currentSrc;
    const explicit = video.getAttribute('src') ? video.src : '';
    if (current && explicit && explicit !== current) return;
    const selected = current || explicit;
    const source = xCdnUrl(selected, 'video.twimg.com');
    if (selected && !source && !(selected.startsWith('blob:') && new URL(selected).origin === location.origin)) return;
    const ownPoster = xCdnUrl(video.poster, 'pbs.twimg.com');
    const records = [...cache.values()];
    const posters = xComponentPosters(video, records);
    if (ownPoster) posters.push(ownPoster);
    const posterKeys = new Set(posters.map(xPosterKey));
    const posterMediaKeys = new Set(posters.map(xCdnKey).filter(Boolean));
    if (posterMediaKeys.size > 1) return;
    const posterMedia = [...posterMediaKeys][0];
    const sourceMedia = xCdnKey(source);
    if (sourceMedia && posterMedia && sourceMedia !== posterMedia) return;
    const cdnMedia = sourceMedia || posterMedia;
    const hasPosterMedia = rec => posterMedia && (rec.posterMediaKeys.has(posterMedia) ||
      rec.sourceKey === posterMedia || (!rec.sourceKey && rec.declaredId === posterMedia.split('/')[1]));
    const byPoster = records.filter(rec => [...posterKeys].some(key => rec.posterKeys.has(key)) || hasPosterMedia(rec));
    if (byPoster.length > 1 && posterKeys.size > 1) return;
    let matches = byPoster;
    if (source) {
      const bySource = records.filter(rec => rec.formats.some(row => row.url === source.href) ||
        (sourceMedia && (rec.sourceKey === sourceMedia || (!rec.sourceKey && rec.declaredId === sourceMedia.split('/')[1]))));
      // A recycled player can momentarily retain the previous video's poster.
      if (byPoster.length && bySource.length && !bySource.some(rec => byPoster.includes(rec))) return;
      matches = byPoster.length || posterMedia ? bySource.filter(rec => byPoster.includes(rec)) : bySource;
    }
    // The site's canonical per-attachment link can wrap the selected video.
    // Post permalinks and unrelated links elsewhere in the card are ambiguous.
    const link = xAttachmentLink(video.closest('a[href]')?.href);
    if (link) {
      const linked = records.filter(rec => rec.attachmentLink === link);
      if (!linked.length && matches.some(rec => rec.attachmentLink && rec.attachmentLink !== link)) return;
      if (linked.length) {
        matches = matches.length ? matches.filter(rec => linked.includes(rec)) :
          (!source && !posterMedia ? linked : []);
        if (!matches.length) return;
      }
    }
    if (matches.length > 1) return;
    if (!matches.length) {
      if (!cdnMedia || (source && (!sourceMedia || byPoster.length))) return;
      return {mediaId:'cdn:' + cdnMedia, extractorUrl:xExtractorUrl(link), xCdnMedia:cdnMedia, formats:[]};
    }
    const rec = matches[0];
    if (posterMedia && !hasPosterMedia(rec)) return;
    const result = {mediaId:rec.mediaId, formats:rec.formats};
    result.extractorUrl = xExtractorUrl(rec.attachmentLink || link);
    if (cdnMedia && (!rec.sourceKey || rec.sourceKey === cdnMedia)) {
      // Metadata can arrive while the source menu is open. The selected
      // player already had this identity before the response was available.
      result.mediaId = 'cdn:' + cdnMedia;
      result.xCdnMedia = cdnMedia;
    }
    if (source && current && rec.formats.filter(row => row.url === current).length === 1) {
      result.formats = rec.formats.map(row => row.url === source.href ? {...row, current:true} : row);
    }
    return result;
  }
  function inspect(video) {
    if (embedId) return inspectXEmbed(video);
    if (x) return inspectX(video);
    if (yt) {
      const p = video.closest('.html5-video-player');
      if (!p || p.classList.contains('ad-showing') || p.classList.contains('ad-interrupting')) return;
      const videos = [...p.querySelectorAll('video')].filter(el => el.closest('.html5-video-player') === p);
      if (videos.length > 1) {
        const main = videos.filter(el => el.classList.contains('html5-main-video'));
        if (main.length !== 1 || main[0] !== video) return;
      }
      const id = p.getVideoData?.()?.video_id;
      if (!id) return;
      const extractorUrl = /^[\w-]{11}$/.test(id) ? 'https://www.youtube.com/watch?v=' + id : undefined;
      const current = p.getPlayerResponse?.();
      const matching = current?.videoDetails?.videoId === id ? current : null;
      if (matching && youtubeUnavailable(matching)) return;
      const active = youtubeRecord(matching);
      const activeRows = [...list(matching?.streamingData?.formats), ...list(matching?.streamingData?.adaptiveFormats)];
      if (activeRows.some(row => row.drmFamilies?.length || row.licenseInfos?.length)) {
        // With mixed clear/protected formats only explicit, current clear files
        // or track pairs identify a transferable rendition.
        const formats = active.formats.filter(f => f.kind === 'direct' || f.kind === 'pair');
        return { ...active, formats };
      }
      const records = [active, cache.get(id),
        window.ytInitialPlayerResponse?.videoDetails?.videoId === id && youtubeRecord(window.ytInitialPlayerResponse)].filter(Boolean);
      if (!records.length) {
        if (current?.videoDetails?.videoId && !matching) return;
        return extractorUrl ? {mediaId:'yt:' + id, extractorUrl, formats:[]} : undefined;
      }
      const formats = new Map();
      const heights = new Set();
      for (const record of records) {
        for (const candidate of record.formats) if (!formats.has(candidate.id)) formats.set(candidate.id, candidate);
        for (const height of record.reportedHeights) heights.add(height);
      }
      return {...records[0], extractorUrl, formats:[...formats.values()].sort(byHeight), reportedHeights:[...heights].sort((a,b) => b-a)};
    }
    return inspectBili(video);
  }
  document.addEventListener('vd-inspect-player-v1', (event) => {
    if (embedId ? !(event.target instanceof HTMLImageElement) : !element(event.target)) return;
    try {
      const rec = inspect(event.target);
      if (!(rec?.formats.length || rec?.reportedHeights?.length || rec?.xCdnMedia || rec?.playbackTransport || rec?.extractorUrl)) return;
      const out = {title: typeof rec.title === 'string' ? rec.title.slice(0, 256) : '', formats: rec.formats.slice(0, 48), reportedHeights: (rec.reportedHeights || []).slice(0, 32)};
      if (rec.mediaId) out.mediaId = rec.mediaId;
      if (rec.extractorUrl) out.extractorUrl = rec.extractorUrl;
      if (rec.playbackTransport === 'sabr') out.playbackTransport = rec.playbackTransport;
      if (rec.xCdnMedia) out.xCdnMedia = rec.xCdnMedia;
      event.target.dispatchEvent(new CustomEvent('vd-player-formats-v1', {detail: JSON.stringify(out)}));
    } catch { /* An unknown player version has no attributable media here. */ }
  }, true);
})();
