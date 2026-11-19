import test from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import {runInNewContext} from 'node:vm';
import {createHash, randomUUID} from 'node:crypto';
import {JSDOM} from 'jsdom';

const read = file => readFileSync(new URL('../' + file, import.meta.url), 'utf8');
const manifest = JSON.parse(read('manifest.json'));
const pageUrl = 'https://x.com/demo/status/100';
const mediaUrl = n => 'https://video.twimg.com/video/' + n + '.mp4';
const sender = {id:'extension-id', url:pageUrl, documentId:'document-1', tab:{id:1}};
const player = randomUUID();
const api = {URL};
runInNewContext(read('media.js'), api);
const {elementSources, hlsVariants, resourceUrl, collectFormats} = api.VDMedia;

function worker({manifestBody, fetchMedia, native = async () => ({ok:true}), now = () => Date.now(), stored = {}} = {}) {
  const drafts = [];
  const callbacks = {};
  const context = {
    URL, AbortController, TextDecoder, setTimeout, clearTimeout, crypto:{randomUUID}, Date:{now},
    fetch:fetchMedia || (async () => new Response(manifestBody ?? '', {status:200})),
    chrome:{
      storage:{session:{async get(key) {return key === null ? {...stored} : {[key]:stored[key]};},
        async set(values) {Object.assign(stored, values);}, async remove(keys) {for (const key of [].concat(keys)) delete stored[key];}}},
      runtime:{id:sender.id, getManifest:() => manifest,
        onMessage:{addListener(fn) { callbacks.message = fn; }},
        async sendNativeMessage(name, draft) {assert.equal(name,'com.versadown.browser'); drafts.push(draft); return native(draft);}},
      webRequest:{onHeadersReceived:{addListener(fn) {callbacks.observe = fn;}}},
      i18n:{getMessage:(key, sub) => sub ? key + ' ' + sub.join(' ') : key},
    },
    importScripts() {},
  };
  runInNewContext(read('media.js'), context);
  runInNewContext(read('background.js'), context);
  return {drafts, observe:callbacks.observe,
    send:async (data, source = sender) => new Promise(resolve => callbacks.message({action:'discover', player, pageUrl:source.url, ...data},source,resolve))};
}

test('native format enumeration targets the selected video and selection submits only an opaque preview choice', async () => {
  const previewId = randomUUID();
  const backend = worker({native:async draft => draft.action === 'resolve'
    ? {ok:true,previewId,choices:[{id:'232+140',height:720,ext:'mp4'}, {id:'18',height:360,ext:'mp4'}]}
    : {ok:true}});
  const exact = 'https://x.com/i/web/status/100/video/2';
  const menu = await backend.send({action:'resolveVideo',targets:[{url:exact,kind:'page'}]});
  assert.deepEqual(Array.from(menu.options, row => row.label), ['720p · MP4','360p · MP4']);
  assert.equal(backend.drafts.length,1);
  assert.equal(backend.drafts[0].pageUrl,exact);
  const pick = {action:'downloadVideo',handle:menu.options[0].handle};
  const differentDocument = {...sender,documentId:'different'};
  assert.equal((await backend.send(pick,differentDocument)).reason,'selectionExpired');
  assert.equal((await backend.send(pick)).ok,true);
  const draft=backend.drafts[1];
  assert.equal(draft.version,3);
  assert.equal(draft.previewId,previewId);
  assert.equal(draft.formatId,'232+140');
  assert.equal(draft.pageUrl,undefined);
});

test('unknown sites bind a multi-video page to the selected element, never the page-wide extractor', () => {
  const url='https://video.example.net/example.html';
  const {playbackTargets,canResolveUrl}=api.VDMedia;
  const target = 'https://cdn.example.net/selected.m3u8';
  const sources=[{url:'blob:https://video.example.net/selected',current:true},{url:target,type:'application/vnd.apple.mpegurl'}];
  assert.deepEqual(Array.from(playbackTargets(url,null,sources,false), t => t.url),[target]);
  assert.deepEqual(Array.from(playbackTargets(url,null,sources.slice(0,1),false)),[]);
  assert.deepEqual(Array.from(playbackTargets(url,null,sources,true), t => t.url),[url,target]);
  assert.equal(canResolveUrl('https://video.example.net/different-page',url,'page'),false);
  assert.equal(canResolveUrl('https://x.com/i/web/status/123/video/1',url,'page'),false);
});

test('YouTube native resolution never falls back to a standalone low-quality player URL', () => {
  const url='https://www.youtube.com/watch?v=aaaaaaaaaaa';
  const exact='https://www.youtube.com/watch?v=bbbbbbbbbbb';
  const sources=[{url:'https://sample.googlevideo.com/legacy.mp4',current:true}];
  assert.deepEqual(Array.from(api.VDMedia.playbackTargets(url,{extractorUrl:exact},sources,true), t => t.url),[exact]);
  assert.deepEqual(Array.from(api.VDMedia.playbackTargets(url,null,sources,true)),[]);
});

test('X sends an observed master playlist only when it identifies the selected attachment', async () => {
  const previewId = randomUUID();
  const backend = worker({
    manifestBody:'#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=1280x720\n720.m3u8',
    native:async () => ({ok:true,previewId,choices:[{id:'720',height:720}]}),
  });
  const target = 'https://video.twimg.com/amplify_video/4321/pl/master.m3u8';
  for (const url of [target.replace('4321','43210'), target]) {
    backend.observe({documentId:sender.documentId,tabId:1,method:'GET',statusCode:200,url});
  }
  const result = await backend.send({action:'resolveVideo',xCdnMedia:'amplify_video/4321'});
  assert.equal(result.ok,true);
  assert.deepEqual(Array.from(backend.drafts, draft => draft.pageUrl),[target]);
});

test('YouTube file checks reject CDN errors, empty responses, and playback envelopes before native submission', async () => {
  const yt = {...sender, url:'https://www.youtube.com/watch?v=example'};
  const url = 'https://example.googlevideo.com/videoplayback?itag=18';
  const format = {kind:'direct', url, height:360, label:'360p'};
  const cases = [
    [() => new Response('', {status:403}), 'sourceHttpError'],
    [() => new Response(new Uint8Array(), {headers:{'Content-Type':'video/mp4'}}), 'sourceNotFile'],
    [() => new Response('playback', {headers:{'Content-Type':'application/vnd.yt-ump'}}), 'sourceNotFile'],
    [() => {throw new TypeError('network error');}, 'sourceUnreachable'],
  ];
  for (const [response,reason] of cases) {
    const calls = [];
    const backend = worker({fetchMedia:async (u,options) => {calls.push([u,options]);return response();}});
    const result = await backend.send({formats:[format]},yt);
    assert.equal(calls.length,0);
    const pick = await backend.send({action:'createTask',handle:result.options[0].handle},yt);
    assert.equal(pick.reason,reason);
    if (reason === 'sourceHttpError') assert.equal(pick.status,403);
    assert.equal(calls[0][0],url);
    assert.equal(calls[0][1].headers.Range,'bytes=0-63');
    assert.equal(calls[0][1].credentials,'omit');
    assert.equal(calls[0][1].signal.aborted,true);
    assert.equal(backend.drafts.length,0);
  }
});

test('both YouTube tracks must return file bytes; a lost native ACK can be retried with the same ID', async () => {
  const yt = {...sender, url:'https://www.youtube.com/watch?v=example'};
  const url = 'https://example.googlevideo.com/videoplayback?itag=137', audioUrl=url.replace('137','140');
  let failAudio=true,failHost=true;
  const probes=[];
  const backend=worker({fetchMedia:async u => {
    probes.push(u);
    if (u===audioUrl && failAudio) return new Response('denied',{status:403});
    return new Response(Uint8Array.of(0,0,0,20,102,116,121,112,105,115,111,109),{status:206});
  }, native:async () => {if (failHost) throw Error('lost ACK'); return {ok:true};}});
  const {options}=await backend.send({formats:[{kind:'pair',url,audioUrl,height:1080,label:'1080p'}]},yt);
  const action={action:'createTask',handle:options[0].handle};
  assert.equal((await backend.send(action,yt)).reason,'sourceHttpError');
  assert.deepEqual(probes,[url,audioUrl]);
  assert.equal(backend.drafts.length,0);
  failAudio=false;
  assert.equal((await backend.send(action,yt)).reason,'hostUnavailable');
  failHost=false;
  assert.equal((await backend.send(action,yt)).ok,true);
  assert.equal(probes.length,4);
  assert.equal(backend.drafts[0].requestId,backend.drafts[1].requestId);
});

test('observations for another video in the same frame cannot enter the clicked draft', async () => {
  const backend = worker();
  for (const id of [1,2]) backend.observe({documentId:sender.documentId, tabId:sender.tab.id,
    method:'GET', statusCode:200, url:mediaUrl(id), responseHeaders:[{name:'Content-Type', value:'video/mp4'}]});
  const second = await backend.send({sources:[{url:mediaUrl(2),current:true}], height:720, title:'Second video'});
  assert.equal(backend.drafts.length,0);
  assert.equal(second.options.length,1);
  assert.match(second.options[0].label,/720p.*currentSource/);
  await backend.send({action:'createTask',handle:second.options[0].handle});
  assert.equal(backend.drafts.length,1);
  const draft = backend.drafts[0];
  assert.equal(draft.version,2);
  assert.equal(draft.title,'Second video');
  assert.equal(draft.formats.length,1);
  assert.equal(draft.formats[0].url, mediaUrl(2));
  assert.equal(draft.formats[0].height,720);
  assert.equal(draft.formats[0].evidence,'observed');
  const unmatched = await backend.send({sources:[{url:'blob:https://x.com/1'}]});
  assert.equal(unmatched.reason,'noMedia');
  assert.equal(backend.drafts.length,1);
});

test('public X embed handoff is scoped to its post and video CDN', async () => {
  const embed='https://platform.twitter.com/embed/Tweet.html?id=123';
  assert.equal(api.VDMedia.xPublicEmbedUrl('https://x.com/someone/status/123/video/2?source=web'),embed);
  assert.equal(api.VDMedia.xPublicEmbedUrl('https://x.com/home'),null);
  assert.equal(api.VDMedia.xPublicEmbedUrl('https://x.com.example.test/user/status/123'),null);
  assert.equal(resourceUrl('https://platform.twitter.com/embed/Tweet.html?id=124',embed),null);
  assert.equal(resourceUrl('https://pbs.twimg.com/avatar.jpg',embed),null);
  assert.equal(api.VDMedia.xEmbedPostId(embed+'&id=456'),null);
  const s={...sender,url:embed};
  const backend=worker();
  const {options}=await backend.send({formats:[{kind:'direct',url:mediaUrl(3),height:720,label:'720p'}]},s);
  assert.equal(options.length,1);
  assert.equal(backend.drafts.length,0);
  await backend.send({action:'createTask',handle:options[0].handle},s);
  assert.equal(backend.drafts[0].pageUrl,embed);
  assert.equal(backend.drafts[0].formats[0].url,mediaUrl(3));
});

test('explicit src overrides stale child sources during an SPA navigation', () => {
  const doc = new JSDOM('<video src="blob:https://x.com/2"><source src="https://video.twimg.com/1.mp4"></video>', {url:pageUrl});
  const video = doc.window.document.querySelector('video');
  assert.deepEqual(Array.from(elementSources(video), s => s.url), ['blob:https://x.com/2']);
});

test('history navigation can discover and submit on the current same-origin frame route', async () => {
  const backend = worker();
  const current = 'https://x.com/demo/status/101';
  const menu = await backend.send({pageUrl:current,sources:[{url:mediaUrl(2),current:true}]});
  assert.equal(menu.options.length,1);
  const pick = {action:'createTask',handle:menu.options[0].handle};
  assert.equal((await backend.send({...pick,pageUrl:'https://x.com/demo/status/102'})).reason,'selectionExpired');
  assert.equal((await backend.send({...pick,pageUrl:'https://video.twimg.com/elsewhere'})).reason,'changedMedia');
  assert.equal((await backend.send({...pick,pageUrl:current})).ok,true);
  assert.equal(backend.drafts[0].pageUrl,current);
});

test('X network fallback matches the exact attachment and excludes fragment, audio and unrelated media', async () => {
  const backend = worker({manifestBody:'#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6,\nsilent.m4s'});
  const cdn = 'https://video.twimg.com/';
  const target = cdn + 'amplify_video/4321/vid/avc1/720x1280/clip.mp4';
  for (const url of [target, cdn+'amplify_video/43210/vid/avc1/720x1280/other.mp4',
    cdn+'ext_tw_video/4321/vid/avc1/720x1280/collision.mp4', cdn+'amplify_video/4321/pl/audio/audio.m3u8',
    cdn+'amplify_video/4321/pl/only-a-track.m3u8', cdn+'amplify_video/4321/vid/avc1/720x1280/partial.mp4?range=1-2']) {
    backend.observe({documentId:sender.documentId,tabId:1,method:'GET',statusCode:200,url});
  }
  const menu = await backend.send({xCdnMedia:'amplify_video/4321',sources:[]});
  assert.equal(menu.options.length,1);
  assert.equal(menu.options[0].label,'720×1280');
  assert.equal(backend.drafts.length,0);
  await backend.send({action:'createTask',handle:menu.options[0].handle});
  assert.equal(backend.drafts[0].formats[0].url,target);
  const resumed = worker();
  const restored = await resumed.send({xCdnMedia:'amplify_video/4321',xDocumentUrls:[target,
    cdn+'ext_tw_video/4321/vid/avc1/720x1280/other.mp4']});
  assert.equal(restored.options.length,1);
  assert.equal(restored.options[0].label,'720×1280');
});

test('HLS names independent renditions by dimensions and published average or peak bandwidth', () => {
  const codecs = 'CODECS="avc1.42,mp4a.40.2"';
  const playlist = ['#EXTM3U',
    '#EXT-X-STREAM-INF:' + codecs + ',RESOLUTION=720x1280,BANDWIDTH=2100000,AVERAGE-BANDWIDTH=1700000', 'portrait',
    '#EXT-X-STREAM-INF:' + codecs + ',BANDWIDTH=900000', 'opaque',
    '#EXT-X-STREAM-INF:' + codecs, 'unknown',
    '#EXT-X-STREAM-INF:' + codecs + ',AUDIO="external",BANDWIDTH=9900000', 'detached',
  ].join('\n');
  const formats = hlsVariants(playlist, 'https://video.twimg.com/master', pageUrl);
  assert.deepEqual(Array.from(formats, row => [row.label, row.height]),[
    ['720×1280 · 1700 kbps',1280], ['900 kbps',undefined],
  ]);
});

test('adaptive HLS advertises only fixed self-contained AV renditions beside the master', async () => {
  const body = [
    '#EXTM3U',
    '#EXT-X-STREAM-INF:BANDWIDTH=500,RESOLUTION=640x360,CODECS="avc1.42,mp4a.40.2"', '360.m3u8',
    '#EXT-X-STREAM-INF:BANDWIDTH=1000,RESOLUTION=1280x720,CODECS="avc1.42,mp4a.40.2",AUDIO="en"', 'silent.m3u8',
    '#EXT-X-STREAM-INF:BANDWIDTH=2000,RESOLUTION=1920x1080,CODECS="avc1.42"', 'video.m3u8',
    '#EXT-X-STREAM-INF:BANDWIDTH=3000,RESOLUTION=1920x1080,CODECS="avc1.42,mp4a.40.2"', 'http://127.0.0.1/private.m3u8'
  ].join('\n');
  const master = 'https://video.twimg.com/media/index.m3u8';
  const picks = hlsVariants(body, master, pageUrl);
  assert.deepEqual(Array.from(picks, f => [f.height,f.url]), [[360, 'https://video.twimg.com/media/360.m3u8']]);
  const backend = worker({manifestBody:body});
  const result = await backend.send({sources:[{url:master, current:true}], height:720});
  assert.equal(result.options.length,2);
  assert.match(result.options[0].label,/HLS.*automatic/);
  assert.doesNotMatch(result.options[1].label,/currentSource/);
  assert.equal(backend.drafts.length,0);
  await backend.send({action:'createTask', handle:result.options[1].handle});
  const [format] = backend.drafts[0].formats;
  assert.equal(format.evidence,'manifest');
  assert.equal(format.height,360);
});

test('unmatched frame-wide manifests are never downloadable from a player', async () => {
  const backend = worker();
  const url = 'https://video.twimg.com/pull?sig=never-return-this';
  backend.observe({documentId:sender.documentId, tabId:sender.tab.id, method:'GET', statusCode:200, url,
    responseHeaders:[{name:'Content-Type',value:'application/dash+xml'}]});
  const result = await backend.send({sources:[{url:'blob:https://x.com/ambiguous'}]});
  assert.equal(result.reason,'unboundManifest');
  assert.equal(result.options,undefined);
  assert.ok(!JSON.stringify(result).includes('never-return-this'));
  assert.equal(backend.drafts.length,0);
});

test('session choices survive worker suspension and pin the selected player, document, URL and rendition', async () => {
  let clock = 200;
  const stored = {}, first = worker({stored,now:()=>clock});
  const formats = [360,720].map(height => ({kind:'direct',url:mediaUrl(height),height,label:height + 'p',evidence:'player'}));
  const menu = await first.send({formats, title:'clicked'});
  assert.equal(first.drafts.length,0);
  assert.deepEqual(Array.from(menu.options, c=>c.label),['360p','720p']);
  assert.ok(!JSON.stringify(menu).includes('twimg.com'));
  const next = worker({stored,now:()=>clock});
  const pick = {action:'createTask',handle:menu.options[1].handle, formats:[formats[0]],title:'spoof'};
  for (const other of [{...sender,documentId:'doc2'},{...sender,tab:{id:2}},{...sender,url:pageUrl+'/2'}]) {
    assert.equal((await next.send(pick,other)).reason,'selectionExpired');
  }
  assert.equal((await next.send({...pick,player:randomUUID()})).reason,'selectionExpired');
  assert.equal(next.drafts.length,0);
  const responses = await Promise.all([next.send(pick),next.send(pick)]);
  assert.equal(responses.filter(r=>r.ok).length,1);
  assert.equal(next.drafts.length,1);
  assert.equal(next.drafts[0].title,'clicked');
  assert.deepEqual(Array.from(next.drafts[0].formats,f=>f.url),[mediaUrl(720)]);
  assert.equal((await next.send(pick)).reason,'selectionExpired');
  const refreshed = await next.send({formats});
  clock += 121000;
  assert.equal((await next.send({...pick,handle:refreshed.options[0].handle})).reason,'selectionExpired');
});

test('reported-only qualities and protected manifests stay out of the selection list', async () => {
  const backend = worker({manifestBody:'#EXTM3U\n#EXT-X-KEY:METHOD=SAMPLE-AES,KEYFORMAT="protected"\n'});
  const unavailable = await backend.send({reportedHeights:[1080,144,720,720,0,'2160'], sources:[{url:'blob:https://x.com/1'}]});
  assert.equal(unavailable.reason,'reportedOnly');
  assert.deepEqual(Array.from(unavailable.reportedOnly), [144,720,1080]);
  const mixed = await backend.send({reportedHeights:[720,1080], formats:[{kind:'direct',url:mediaUrl(2),height:720,evidence:'player'}]});
  assert.deepEqual(Array.from(mixed.reportedOnly),[1080]);
  assert.equal(mixed.options.length,1);
  const blocked = await backend.send({sources:[{url:'https://video.twimg.com/media/index.m3u8'}]});
  assert.equal(blocked.reason,'noMedia');
  assert.equal((await backend.send({action:'createTask',handle:mixed.options[0].handle})).reason,'selectionExpired');
  assert.equal(backend.drafts.length,0);
});

test('native failures are reported only after selection and retries reuse the native request ID', async () => {
  let calls = 0;
  const backend = worker({native:async () => { if (++calls === 1) throw new Error('missing host'); return {ok:calls > 2}; }});
  const result = await backend.send({sources:[{url:mediaUrl(2), current:true}], height:720, formats:[
    {kind:'direct',url:mediaUrl(3),height:1280,label:'720×1280 · 2100 kbps',evidence:'player'},
    {kind:'direct',url:mediaUrl(3),height:1280,label:'720×1280 · 2100 kbps',evidence:'player'},
  ]});
  assert.equal(result.options.length,2);
  assert.equal(result.options[0].label,'720×1280 · 2100 kbps');
  assert.match(result.options[1].label,/currentSource/);
  assert.equal(calls,0);
  const pick = {action:'createTask',handle:result.options[0].handle};
  assert.equal((await backend.send(pick)).reason,'hostUnavailable');
  assert.equal((await backend.send(pick)).reason,'hostRejected');
  assert.equal((await backend.send(pick)).ok,true);
  assert.equal(new Set(backend.drafts.map(d=>d.requestId)).size,1);
});

test('untrusted page formats cannot supply unrelated origins, local targets or partial byte URLs', () => {
  for (const url of ['file:///tmp/v.mp4','http://127.0.0.1/v.mp4','https://x.com.bad.test/v.mp4',
    'https://user:password@video.twimg.com/v.mp4','https://www.youtube.com/v.mp4']) {
    assert.equal(resourceUrl(url,pageUrl),null);
  }
  const group = [
    {kind:'pair',url:mediaUrl(2),audioUrl:mediaUrl(2)},
    {kind:'pair',url:mediaUrl(2),audioUrl:'https://localhost/audio.mp4'},
    {kind:'direct',url:mediaUrl(3)+'?range=0-2034'},
    {kind:'direct',url:'https://video.twimg.com/fragment.m4s'},
    {kind:'pair',url:mediaUrl(2),audioUrl:mediaUrl(3),height:720,evidence:'player'},
  ];
  const sanitized = collectFormats(group,pageUrl);
  assert.equal(sanitized.length,1);
  assert.equal(sanitized[0].kind,'pair');
  assert.equal(sanitized[0].audioUrl,mediaUrl(3));
});

test('Bilibili full DASH .m4s player track pair survives while observed fragments and partial ranges are excluded', () => {
  const bilibili = 'https://www.bilibili.com/video/BVdemo';
  const url = 'https://upos-sz-mirror.bilivideo.com/path/123-1-30112.m4s?deadline=10';
  const audioUrl = 'https://upos-sz-mirror.bilivideo.com/path/123-1-30280.m4s?deadline=10';
  const pair = {kind:'pair', url, audioUrl, height:1080, evidence:'player'};
  const formats = collectFormats([pair, {...pair,evidence:'observed'}, {...pair,kind:'direct'}, {...pair,url:url+'&range=2-8'}], bilibili);
  assert.equal(formats.length,1);
  assert.equal(formats[0].audioUrl,audioUrl);
  assert.equal(api.VDMedia.mediaKind(url, 'video/mp4'), null);
});

test('overlay binds each visible video and rejects clicks synthesized by the host page', () => {
  const {window} = new JSDOM('<video src="/first.mp4"></video><video src="/second.mp4"></video>',
    {url:'http://localhost/watch', runScripts:'outside-only', pretendToBeVisual:true});
  const shadowRoots = [];
  const attach = window.Element.prototype.attachShadow;
  window.Element.prototype.attachShadow = function (options) {
    const root = attach.call(this,options);
    shadowRoots.push(root);
    return root;
  };
  window.ResizeObserver = class {observe() {} unobserve() {}};
  let sent = 0;
  window.chrome = {i18n:{getMessage:key => key},runtime:{sendMessage() {sent++;}}};
  for (const [i, video] of [...window.document.querySelectorAll('video')].entries()) {
    video.getBoundingClientRect = () => ({top:i*180,left:0,bottom:i*180+160,right:320,height:160,width:320});
  }
  window.eval(read('media.js'));
  window.eval(read('content.js'));
  assert.equal(shadowRoots.length,2);
  assert.equal(shadowRoots[1].host.style.top,'188px');
  shadowRoots[1].querySelector('button').click();
  assert.equal(sent,0);
  window.close();
});

test('a single source is rendered as a choice and a recycled player cannot submit its old source', async () => {
  const {window} = new JSDOM('<video src="/second.mp4"></video>', {url:'http://localhost/watch', runScripts:'outside-only', pretendToBeVisual:true});
  const video = window.document.querySelector('video');
  video.getBoundingClientRect = () => ({top:0,left:0,bottom:180,right:320,height:180,width:320});
  window.ResizeObserver = class {observe() {} unobserve() {}};
  let shadow, mediaId = 'A';
  const shadowMethod = window.Element.prototype.attachShadow;
  window.Element.prototype.attachShadow = function(options) { return shadow = shadowMethod.call(this, options); };
  // jsdom has no native user input. Capture the registered handlers separately
  // from the preceding test of the DOM's untrusted-event boundary.
  const clicks = new Map(), add = window.HTMLButtonElement.prototype.addEventListener;
  window.HTMLButtonElement.prototype.addEventListener = function(type, fn, ...rest) {
    if (type === 'click') clicks.set(this,fn);
    return add.call(this,type,fn,...rest);
  };
  const click = button => clicks.get(button)({isTrusted:true, stopPropagation() {}, preventDefault() {}});
  const messages = [];
  window.VDSiteMedia = {inspect:() => ({mediaId, formats:[]})};
  window.chrome = {i18n:{getMessage:key => key},runtime:{async sendMessage(message) {
    messages.push(message);
    return message.action === 'resolveVideo' ? {ok:true,options:[{handle:'opaque',label:'640×360 · 700 kbps'}]} : {ok:true};
  }}};
  window.eval(read('media.js'));
  window.eval(read('content.js'));
  try {
    const opener = shadow.querySelector('button'), menu = shadow.querySelector('.choices');
    await click(opener);
    assert.deepEqual(messages.map(m=>m.action),['resolveVideo']);
    assert.equal(menu.hidden,false);
    assert.equal(menu.children.length,1);
    assert.equal(menu.firstChild.textContent,'640×360 · 700 kbps');
    // Adaptive playback can replace a blob URL while preserving the player ID.
    video.src = 'blob:http://localhost/new-pipeline';
    video.dispatchEvent(new window.Event('loadstart'));
    assert.equal(menu.hidden,false);
    mediaId = 'B';
    await click(menu.firstChild);
    assert.equal(messages.length,1);
    assert.equal(shadow.querySelector('output').textContent,'changedMedia');
    assert.equal(menu.hidden,true);
    await click(opener);
    await click(menu.firstChild);
    assert.deepEqual(messages.map(m=>m.action),['resolveVideo','resolveVideo','downloadVideo']);
    assert.equal(messages[2].handle,'opaque');
    assert.equal(messages[2].formats,undefined);
    assert.equal(shadow.querySelector('output').textContent,'taskCreated');
  } finally { window.close(); }
});

test('every declared script, translation and stable Chrome native host identity can be loaded', () => {
  for (const item of manifest.content_scripts) for (const script of item.js) assert.ok(read(script).length);
  const zh = JSON.parse(read('_locales/zh_CN/messages.json'));
  const en = JSON.parse(read('_locales/en/messages.json'));
  assert.deepEqual(Object.keys(zh),Object.keys(en));
  assert.equal(zh.downloadButton.message,'万能下载+');
  const id = createHash('sha256').update(Buffer.from(manifest.key, 'base64')).digest('hex').slice(0,32)
    .replace(/[0-9a-f]/g, digit => String.fromCharCode(97 + Number.parseInt(digit,16)));
  assert.equal(id,'koomjmkmehhhpkncngdkpkbfbjjbnccf');
});
