// Pure helpers shared by the isolated content script and the extension worker.
(() => {
  const hostIn = (host, domain) => host === domain || host.endsWith('.' + domain);
  const families = [
    ['bilibili.com', 'bilivideo.com', 'bilivideo.cn', 'hdslb.com'],
    ['youtube.com', 'googlevideo.com'],
    ['x.com', 'video.twimg.com'],
  ];

  function xEmbedPostId(value) {
    try {
      const u = new URL(value);
      const ids = u.searchParams.getAll('id');
      if (u.origin === 'https://platform.twitter.com' && !u.username && !u.password && u.pathname === '/embed/Tweet.html' &&
          ids.length === 1 && /^\d{1,20}$/.test(ids[0])) return ids[0];
    } catch { /* Not a public post embed. */ }
    return null;
  }

  function xPublicEmbedUrl(value) {
    try {
      const u = new URL(value);
      if (u.protocol !== 'https:' || !['x.com','www.x.com'].includes(u.hostname) || u.port || u.username || u.password) return null;
      const match = /^\/(?:[A-Za-z0-9_]+|i\/web)\/status\/(\d{1,20})(?:\/video\/[1-9]\d*)?\/?$/.exec(u.pathname);
      return match ? 'https://platform.twitter.com/embed/Tweet.html?id=' + match[1] : null;
    } catch { return null; }
  }

  function resourceUrl(value, pageUrl) {
    if (typeof value !== 'string' || value.length > 8192) return null;
    try {
      const page = new URL(pageUrl);
      const url = new URL(value);
      if (!['http:', 'https:'].includes(url.protocol) || url.username || url.password) return null;
      if (['localhost', '127.0.0.1'].includes(page.hostname)) return url.origin === page.origin ? url.href : null;
      if (url.protocol !== 'https:' || url.port) return null;
      if (page.hostname === 'platform.twitter.com') {
        const id = xEmbedPostId(page.href);
        if (!id) return null;
        return url.hostname === 'video.twimg.com' || xEmbedPostId(url.href) === id ? url.href : null;
      }
      const family = families.find(domains => hostIn(page.hostname, domains[0]));
      return family?.some(domain => hostIn(url.hostname, domain)) ? url.href : null;
    } catch {
      return null;
    }
  }

  function isFragment(url, explicitBiliTrack = false) {
    const parsed = new URL(url);
    if (['range', 'sq', 'segment', 'sequence'].some(key => parsed.searchParams.has(key))) return true;
    // Bilibili's DASH playinfo can declare a complete byte-addressable track
    // whose filename ends in .m4s. Only an explicit player track pair gets
    // that exception. A network observation of the same suffix is ambiguous.
    if (/\.m4s$/i.test(parsed.pathname)) return !explicitBiliTrack;
    return /\.(?:ts|cmfa|cmfv)$/i.test(parsed.pathname);
  }

  function mediaKind(url, type = '') {
    const parsed = new URL(url);
    const mime = type.split(';')[0].trim().toLowerCase();
    if (mime.includes('mpegurl') || /\.m3u8$/i.test(parsed.pathname)) return 'hls';
    if (mime === 'application/dash+xml' || /\.mpd$/i.test(parsed.pathname)) return 'dash';
    // A player's fragment (or a URL restricted to a byte range) is not a file.
    if (isFragment(url)) return null;
    if (['video/mp4', 'video/webm', 'video/ogg', 'video/quicktime'].includes(mime) ||
        /\.(?:mp4|webm|ogv|mov)$/i.test(parsed.pathname)) return 'direct';
    return null;
  }

  function elementSources(video) {
    const sources = [];
    const selected = video.currentSrc;
    // HTML ignores child sources while an explicit src is active. SPAs can
    // leave the old children in place when switching to an MSE player.
    const children = video.getAttribute('src') ? [] : video.querySelectorAll('source[src]');
    for (const node of children) {
      sources.push({url: node.src, type: node.type, current: node.src === selected});
    }
    if (selected) sources.push({url: selected, current: true});
    else if (video.getAttribute('src')) sources.push({url: video.src, current: false});
    return sources;
  }

  function webUrl(value) {
    if (typeof value !== 'string' || !value || value.length > 8192) return null;
    try {
      const url = new URL(value);
      return ['http:', 'https:'].includes(url.protocol) && !url.username && !url.password ? url : null;
    } catch {
      return null;
    }
  }

  function canResolveUrl(value, from, kind) {
    const url = webUrl(value), page = webUrl(from);
    if (!url || !page) return false;
    if (kind === 'media') {
      return !isFragment(url.href) &&
        (!families.some(([host]) => hostIn(page.hostname, host)) && !xEmbedPostId(from) ||
          !!resourceUrl(url.href, page.href));
    }
    if (kind !== 'page') return false;
    if (hostIn(page.hostname, 'youtube.com')) {
      return url.origin === 'https://www.youtube.com' && url.pathname === '/watch' &&
        /^[\w-]{11}$/.test(url.searchParams.get('v') || '');
    }
    if (hostIn(page.hostname, 'bilibili.com')) {
      return url.origin === 'https://www.bilibili.com' && /^\/video\/(?:BV[\w]+|av\d+)\/$/i.test(url.pathname);
    }
    if (hostIn(page.hostname, 'x.com') || xEmbedPostId(from)) {
      const status = /^\/i\/web\/status\/(\d{1,20})\/video\/[1-4]$/.exec(url.pathname);
      const embed = xEmbedPostId(from);
      return url.origin === 'https://x.com' && !!status && (!embed || status[1] === embed);
    }
    return url.href === page.href;
  }

  function playbackTargets(page, site, sources, onlyPlayer) {
    const targets = [];
    const add = (url, kind) => {
      if (canResolveUrl(url, page, kind) && !targets.some(target => target.url === url)) targets.push({url, kind});
    };
    if (site?.extractorUrl) add(site.extractorUrl, 'page');
    const host = new URL(page).hostname;
    // A watch page, feed or post URL alone cannot bind these site players.
    const known = families.some(([root]) => hostIn(host, root)) || xEmbedPostId(page);
    if (!known && onlyPlayer) add(page, 'page');
    // A fallback address still has to come from this exact element or its paired site record.
    // For YouTube a progressive fallback can be unrelated to the active session.
    if (!hostIn(host, 'youtube.com')) {
      const explicit = sources.flatMap(source => {
        const url = webUrl(source.url);
        const kind = url && mediaKind(url.href, source.type);
        return kind ? [{...source, kind}] : [];
      });
      const manifests = [...explicit, ...(site?.formats || [])].filter(f => f.kind === 'hls' || f.kind === 'dash');
      const direct = explicit.filter(f => f.kind === 'direct');
      const best = manifests.find(f => f.current) || manifests[0] || direct.find(f => f.current) ||
        (new Set(direct.map(f => f.url)).size === 1 && direct[0]);
      if (best) add(best.url, 'media');
    }
    return targets;
  }

  function xAttachmentResource(value, media) {
    if (!/^(ext_tw_video|amplify_video)\/\d{1,24}$/.test(media) ||
        typeof value !== 'string' || value.length > 8192) return;
    let u;
    try { u = new URL(value); } catch { return; }
    if (u.origin !== 'https://video.twimg.com' || u.username || u.password ||
        !u.pathname.startsWith('/' + media + '/') || isFragment(u.href)) return;
    const tail = u.pathname.slice(media.length + 2);
    if (/^(?:pu\/)?pl\/[^/]+\.m3u8$/i.test(tail)) return {url:u.href, kind:'hls', evidence:'observed', label:'HLS'};
    // X's HLS chunks and individual audio playlists can share the attachment's
    // ID. A full MP4 has its own rendition path with explicit dimensions.
    const size = /^(?:pu\/)?vid\/(?:avc1\/)?(\d{2,5})x(\d{2,5})\/[^/]+\.mp4$/i.exec(tail);
    if (size && Number(size[1]) <= 16384 && Number(size[2]) <= 16384)
      return {url:u.href, kind:'direct', height:Number(size[2]), evidence:'observed', label:size[1] + '×' + size[2], mimeType:'video/mp4'};
  }

  function hlsVariants(body, base, pageUrl) {
    if (!body.trimStart().startsWith('#EXTM3U')) return [];
    const lines = body.split(/\r?\n/).map(line => line.trim()).filter(Boolean);
    const variants = [];
    for (let i = 0; i + 1 < lines.length; i++) {
      if (!lines[i].startsWith('#EXT-X-STREAM-INF:') || lines[i + 1].startsWith('#')) continue;
      const attrs = {};
      const list = lines[i].slice('#EXT-X-STREAM-INF:'.length);
      for (const match of list.matchAll(/(?:^|,)([A-Z0-9-]+)=("[^"]*"|[^,]*)/g)) {
        attrs[match[1]] = match[2].replace(/^"|"$/g, '');
      }
      // Preserve external renditions via the master. Offer a child as a fixed
      // quality only when its declaration describes both audio and video.
      const codecs = (attrs.CODECS || '').split(',').map(codec => codec.trim().split('.')[0].toLowerCase());
      if (attrs.AUDIO || attrs.VIDEO || !codecs.some(codec => ['mp4a','opus','ac-3','ec-3','flac'].includes(codec)) ||
          !codecs.some(codec => ['avc1','avc3','hev1','hvc1','av01','vp9','vp09'].includes(codec))) continue;
      const size = /^(\d+)x(\d+)$/i.exec(attrs.RESOLUTION ?? '');
      const width = Number(size?.[1]), height = Number(size?.[2]);
      const hasSize = width > 0 && height > 0 && width <= 16384 && height <= 16384;
      const bandwidth = [attrs['AVERAGE-BANDWIDTH'], attrs.BANDWIDTH]
        .filter(value => /^\d+$/.test(value)).map(Number).find(rate => Number.isSafeInteger(rate) && rate > 0);
      if (!hasSize && !bandwidth) continue;
      let url;
      try { url = resourceUrl(new URL(lines[i + 1], base).href, pageUrl); } catch { continue; }
      if (!url) continue;
      const labels = [];
      if (hasSize) labels.push(width + '×' + height);
      if (bandwidth) labels.push(bandwidth < 1000 ? bandwidth + ' bps' : Math.round(bandwidth / 1000) + ' kbps');
      variants.push({kind:'hls', url, ...(hasSize ? {width, height} : {}), ...(bandwidth ? {bitrate:bandwidth} : {}),
        label:labels.join(' · '), evidence:'manifest'});
    }
    return variants;
  }

  function textLabel(value, fallback, limit) {
    const clean = typeof value === 'string' ? value.replace(/[\u0000-\u001f\u007f-\u009f]/g, ' ').trim() : '';
    return [...(clean || fallback)].slice(0, limit).join('');
  }

  function collectFormats(sourceFormats, pageUrl) {
    const unique = new Map();
    for (const item of sourceFormats) {
      if (!item || !['direct', 'hls', 'dash', 'pair'].includes(item.kind)) continue;
      const url = resourceUrl(item.url, pageUrl);
      const audioUrl = item.kind === 'pair' ? resourceUrl(item.audioUrl, pageUrl) : null;
      if (!url || (item.kind === 'pair' && (!audioUrl || url === audioUrl))) continue;
      const biliPair = item.kind === 'pair' && item.evidence === 'player' && hostIn(new URL(pageUrl).hostname, 'bilibili.com');
      if (['pair', 'direct'].includes(item.kind) && (isFragment(url, biliPair) || (audioUrl && isFragment(audioUrl, biliPair)))) continue;
      const identity = [item.kind, url, audioUrl].join('\n');
      if (unique.has(identity)) continue;
      const entry = {id:'f' + (unique.size + 1), kind:item.kind, url, label:textLabel(item.label, item.kind.toUpperCase(), 80),
        evidence:['player', 'observed', 'manifest'].includes(item.evidence) ? item.evidence : 'player'};
      if (audioUrl) entry.audioUrl = audioUrl;
      if (Number.isInteger(item.height) && item.height > 0 && item.height <= 16384) entry.height = item.height;
      if (typeof item.mimeType === 'string' && /^(video|audio)\/[a-zA-Z0-9.+-]+$/.test(item.mimeType)) entry.mimeType = item.mimeType;
      unique.set(identity, entry);
      if (unique.size >= 48) break;
    }
    return [...unique.values()];
  }
  globalThis.VDMedia = {resourceUrl, mediaKind, elementSources, webUrl, canResolveUrl, playbackTargets, xAttachmentResource, xEmbedPostId, xPublicEmbedUrl,
    hlsVariants, collectFormats, textLabel};
})();
