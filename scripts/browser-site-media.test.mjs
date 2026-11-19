import { readFileSync } from 'node:fs';
import test from 'node:test';
import assert from 'node:assert/strict';
import { JSDOM } from 'jsdom';
const adapter = readFileSync(new URL('../browser-extension/site-media-main.js', import.meta.url), 'utf8');
const isolated = readFileSync(new URL('../browser-extension/site-media.js', import.meta.url), 'utf8');
function page(url, body, playback, prepare) {
  const dom = new JSDOM(body, {url, runScripts:'outside-only'});
  const win = dom.window;
  win.TextDecoder = TextDecoder;
  win.fetch = async (input) => {
    const result = Response.json(playback);
    Object.defineProperty(result, 'url', {value:input});
    return result;
  };
  prepare?.(win);
  win.eval(adapter);
  win.eval(isolated);
  return win;
}
test('Bilibili returns only delivered full tracks for the matching part', () => {
  const win = page('https://www.bilibili.com/video/BVtest/?p=2','<div id="bilibili-player"><video></video></div><video id="unrelated"></video>');
  try {
    win.__INITIAL_STATE__ = {videoData:{bvid:'BVtest',pages:[{page:1,cid:10},{page:2,cid:11}]}};
    win.__playinfo__ = {data:{accept_quality:[120,80,64],support_formats:[{new_description:'1080P 高清'},{new_description:'720P 高清'}],dash:{
      video:[{id:16,height:360,baseUrl:'https://test.bilivideo.com/30016.m4s',mimeType:'video/mp4'},
        {id:80,height:1080,baseUrl:'https://test.bilivideo.com/30112.m4s',mimeType:'video/mp4'}],
      audio:[{baseUrl:'https://test.bilivideo.com/30280.m4s',mimeType:'audio/mp4'}]}}};
    const rec = win.VDSiteMedia.inspect(win.document.querySelector('video'));
    assert.deepEqual(Array.from(rec.formats, row => row.height),[1080,360]);
    assert.deepEqual(new Set(rec.reportedHeights),new Set([1080,720,360]));
    assert.equal(rec.formats[0].kind,'pair');
    assert.equal(rec.extractorUrl,'https://www.bilibili.com/video/BVtest/?p=2');
    assert.equal(win.VDSiteMedia.inspect(win.document.querySelector('#unrelated')),null);
    win.history.pushState({},'', '/video/BVtest/?p=1');
    assert.equal(win.VDSiteMedia.inspect(win.document.querySelector('video')), null);
  } finally {win.close();}
});
test('Bilibili native player binds an embedded video by element and live CID, identifying its exact active track', async () => {
  const base = 'https://upos.test.bilivideo.com/';
  const payload = {data:{dash:{
    audio:[{mimeType:'audio/mp4',baseUrl:base+'audio.m4s'}],
    video:[{mimeType:'video/mp4',height:720,baseUrl:base+'hi.m4s',backupUrl:[base+'ambiguous.m4s']},
      {mimeType:'video/mp4',height:360,baseUrl:base+'lo.m4s',backupUrl:[base+'backup-lo.m4s',base+'ambiguous.m4s']}]}}};
  const win = page('https://player.bilibili.com/player.html?bvid=BVsample&page=1',
    '<section id="shinonome"><video></video></section><video id="preview"></video>',payload);
  try {
    const v = win.document.querySelector('video');
    let cid = 803, playing = base+'backup-lo.m4s,'+base+'audio.m4s';
    win.player = {mediaElement:()=>v, getManifest:()=>({cid}), getMediaInfo:()=>({playUrl:playing})};
    await win.fetch('https://api.bilibili.com/x/player/wbi/playurl?cid=803');
    await new Promise(resolve=>setTimeout(resolve, 15));
    const inspect = () => win.VDSiteMedia.inspect(v);
    assert.equal(inspect().mediaId, 'bili:803');
    assert.deepEqual(Array.from(inspect().formats,f=>[f.height,!!f.current]),[[720,false],[360,true]]);
    assert.equal(win.VDSiteMedia.inspect(win.document.querySelector('#preview')),null);
    playing = base+'not-in-playinfo.m4s';
    assert.equal(inspect().formats.some(f=>f.current),false);
    playing = base+'ambiguous.m4s';
    assert.equal(inspect().formats.some(f=>f.current),false);
    cid++;
    assert.equal(inspect(),null);
  } finally {win.close();}
});
test('Bilibili picks allowed complete mirrors of the same DASH tracks', () => {
  const win = page('https://www.bilibili.com/video/BVmirror/','<div id="bilibili-player"><video></video></div>');
  const valid = 'https://media.bilivideo.com/';
  try {
    win.__INITIAL_STATE__ = {videoData:{cid:123,bvid:'BVmirror'}};
    win.__playinfo__ = {data:{dash:{
      audio:[{mime_type:'audio/mp4',base_url:valid+'a.m4s?range=0-20',backup_url:[valid+'a-full.m4s']}],
      video:[{mime_type:'video/mp4',base_url:'https://media.bilivideo.com:8443/high.m4s',backup_url:[
        'https://unrelated.test/high.m4s',valid+'partial.m4s?segment=2',valid+'high.m4s?token=0,0']},
      {mime_type:'video/mp4',base_url:valid+'fragment.ts',backup_url:[valid+'partial.m4s?range=1-8']}],
    }}};
    const v = win.document.querySelector('video');
    win.player = {mediaElement:()=>v, getManifest:()=>({cid:123}), getMediaInfo:()=>({playUrl:valid+'high.m4s?token=0,0,'+valid+'a-full.m4s'})};
    const rec = win.VDSiteMedia.inspect(v);
    assert.deepEqual(Array.from(rec.formats,f=>[f.url,f.audioUrl,f.current]),
      [[valid+'high.m4s?token=0,0',valid+'a-full.m4s',true]]);
  } finally {win.close();}
});
test('Bilibili keeps its boot response across tracking changes but excludes other parts', () => {
  const win = page('https://www.bilibili.com/video/BVsample/?vd_source=shared',
    '<div id="bilibili-player"><video></video></div>');
  try {
    const v = win.document.querySelector('video');
    win.__INITIAL_STATE__ = {videoData:{bvid:'BVsample',pages:[{page:1,cid:31},{page:2,cid:32}]}};
    win.__playinfo__ = {data:{durl:[{url:'https://upos.test.bilivideo.com/clip.mp4'}],format:'mp4'}};
    win.history.replaceState({},'', '/video/BVsample/#t=30');
    assert.equal(win.VDSiteMedia.inspect(v).mediaId,'bili:31');
    win.history.replaceState({},'', '/video/BVsample/?p=2');
    assert.equal(win.VDSiteMedia.inspect(v),null);
  } finally {win.close();}
});
test('YouTube only associates returned formats with the actual player ID', () => {
  const win = page('https://www.youtube.com/watch?v=second','<div class="html5-video-player"><video></video></div><div class="html5-video-player"><video></video></div>');
  try {
    const players = win.document.querySelectorAll('.html5-video-player');
    players[0].getVideoData = () => ({video_id:'first'});
    players[1].getVideoData = () => ({video_id:'second'});
    win.ytInitialPlayerResponse = {videoDetails:{videoId:'second',title:'Second film'},streamingData:{adaptiveFormats:[
      {itag:140,mimeType:'audio/mp4',url:'https://cdn.googlevideo.com/sound'},
      {itag:136,mimeType:'video/mp4',height:720,url:'https://cdn.googlevideo.com/low'},
      {itag:137,mimeType:'video/mp4',height:1080,url:'https://cdn.googlevideo.com/hd'},
      {itag:315,mimeType:'video/mp4',height:2160,signatureCipher:'unavailable'},
    ]}};
    assert.equal(win.VDSiteMedia.inspect(players[0].querySelector('video')),null);
    const inspect = () => win.VDSiteMedia.inspect(players[1].querySelector('video'));
    assert.deepEqual(Array.from(inspect().formats, f=>f.height),[1080,720]);
    assert.equal(inspect().mediaId,'yt:second');
    assert.deepEqual(Array.from(inspect().reportedHeights),[2160,1080,720]);
    win.ytInitialPlayerResponse.streamingData = {adaptiveFormats:[{itag:137,height:1080},{itag:136,height:720}]};
    assert.equal(inspect().formats.length,0);
    assert.deepEqual(Array.from(inspect().reportedHeights),[1080,720]);
    players[1].classList.add('ad-showing');
    assert.equal(inspect(),null);
  } finally {win.close();}
});
test('YouTube can identify the clicked video even before it has exposed any downloadable format', () => {
  const url = 'https://www.youtube.com/watch?v=aaaaaaaaaaa';
  const win = page(url,'<div class="html5-video-player"><video></video></div>');
  try {
    const player = win.document.querySelector('.html5-video-player');
    player.getVideoData = () => ({video_id:'bbbbbbbbbbb'});
    const record = win.VDSiteMedia.inspect(player.querySelector('video'));
    assert.equal(record.extractorUrl,'https://www.youtube.com/watch?v=bbbbbbbbbbb');
    assert.equal(record.formats.length,0);
  } finally {win.close();}
});
test('YouTube merges delivered renditions for the clicked video without replacing a newer URL or inventing higher tracks', async () => {
  const videoDetails = {videoId:'mixed'};
  const cdn = 'https://cdn.googlevideo.com/';
  const direct = name => ({itag:18, height:360, mimeType:'video/mp4; codecs="avc1,mp4a"', url:cdn + name});
  const cacheResponse = {videoDetails, streamingData:{formats:[direct('older')], adaptiveFormats:[
    {itag:140, mimeType:'audio/mp4', url:cdn+'audio'}, {itag:137, mimeType:'video/mp4', height:1080, url:cdn+'1080'}
  ]}};
  const win = page('https://www.youtube.com/watch?v=mixed', '<div class="html5-video-player"><video></video></div>', cacheResponse);
  try {
    const player = win.document.querySelector('.html5-video-player');
    const current = {videoDetails, streamingData:{formats:[direct('current')], serverAbrStreamingUrl:cdn+'videoplayback?sabr=1'}};
    player.getVideoData = () => ({video_id:'mixed'});
    player.getPlayerResponse = () => current;
    win.ytInitialPlayerResponse = {videoDetails, streamingData:{adaptiveFormats:[{height:2160}]}};
    const inspect = () => win.VDSiteMedia.inspect(player.querySelector('video'));
    await win.fetch('https://www.youtube.com/youtubei/v1/player');
    for (let i=0; i<20 && inspect().formats.length<2; i++) await new Promise(r=>setTimeout(r,5));
    assert.deepEqual(Array.from(inspect().formats,row=>row.url),[cdn+'1080',cdn+'current']);
    assert.deepEqual(Array.from(inspect().reportedHeights),[2160,1080,360]);
    assert.equal(inspect().playbackTransport,'sabr');
    current.playabilityStatus = {status:'UNPLAYABLE'};
    assert.equal(inspect(),null);
    current.playabilityStatus = {status:'OK',liveStreamability:{}};
    current.videoDetails = {videoId:'mixed',isLiveContent:true};
    assert.equal(inspect(),null);
    current.videoDetails = videoDetails;
    current.streamingData.licenseInfos = [{}];
    assert.equal(inspect(),null);
    delete current.streamingData.licenseInfos;
    current.streamingData.adaptiveFormats = [{itag:137,height:1080,drmFamilies:['WIDEVINE']}];
    assert.deepEqual(Array.from(inspect().formats,row=>row.url),[cdn+'current']);
  } finally {win.close();}
});
test('YouTube identifies session-only playback on the main video and keeps an explicit manifest selectable', () => {
  const win = page('https://www.youtube.com/watch?v=film',
    '<div class="html5-video-player"><video class="html5-main-video"></video><video></video></div>');
  try {
    const player = win.document.querySelector('.html5-video-player');
    const [film, auxiliary] = player.querySelectorAll('video');
    player.getVideoData = () => ({video_id:'film'});
    const data = {serverAbrStreamingUrl:'https://cdn.googlevideo.com/videoplayback?sabr=1',
      adaptiveFormats:[{itag:137, height:1080, mimeType:'video/mp4'}]};
    player.getPlayerResponse = () => ({videoDetails:{videoId:'film'}, streamingData:data});
    const inspect = () => win.VDSiteMedia.inspect(film);
    assert.equal(inspect().mediaId, 'yt:film');
    assert.equal(inspect().playbackTransport,'sabr');
    assert.equal(inspect().formats.length,0);
    assert.equal(win.VDSiteMedia.inspect(auxiliary),null);
    data.hlsManifestUrl = 'https://manifest.googlevideo.com/example.m3u8';
    assert.equal(inspect().playbackTransport,undefined);
    assert.deepEqual(Array.from(inspect().formats, f => f.url), [data.hlsManifestUrl]);
    auxiliary.classList.add('html5-main-video');
    assert.equal(inspect(),null);
    auxiliary.remove();
    player.classList.add('ad-interrupting');
    assert.equal(inspect(),null);
  } finally {win.close();}
});
test('X ranks only the clicked media by pixel count then bitrate, preserving its manifest',async () => {
  const base = 'https://video.twimg.com/ext_tw_video/';
  const mp4 = (path, bitrate) => ({content_type:'video/mp4', url:base + path, bitrate});
  const hls = path => ({content_type:'application/x-mpegURL', url:base + path});
  const items = [
    {id_str:'11',video_info:{variants:[hls('11/master')]}},
    {id_str:'22',video_info:{variants:[hls('22/master'), mp4('22/480x1440/clip', 4_000_000),
      mp4('22/720x1280/low', 1_200_000), mp4('22/720x1280/high', 2_000_000)]}},
    {id_str:'33',video_info:{variants:[mp4('33/low', 200_000), mp4('33/opaque'),
      {content_type:'video/mp4; codecs="avc1"',url:base + '33/high',bit_rate:800_000}]}},
  ];
  // A quoted/repeated attachment can omit the attachment id in one copy.
  const duplicate = {video_info:items[1].video_info};
  const win = page('https://x.com/example','<article><video></video><video></video></article>',{items:[...items, duplicate]});
  try {
    const [a,b] = win.document.querySelectorAll('video');
    a.poster='https://pbs.twimg.com/ext_tw_video_thumb/11/img/thumb.jpg';
    b.poster='https://pbs.twimg.com/ext_tw_video_thumb/22/img/thumb.jpg';
    const idBeforeResponse = win.VDSiteMedia.inspect(b).mediaId;
    await win.fetch('https://x.com/i/api/graphql/abc/UserMedia');
    for(let i=0;i<20 && !win.VDSiteMedia.inspect(b)?.formats.length;i++) await new Promise(r=>setTimeout(r,5));
    assert.deepEqual(Array.from(win.VDSiteMedia.inspect(a).formats, row => row.kind),['hls']);
    const formats = win.VDSiteMedia.inspect(b).formats;
    assert.equal(win.VDSiteMedia.inspect(b).mediaId,idBeforeResponse);
    assert.deepEqual(Array.from(formats, row => row.url.slice(base.length)),[
      '22/720x1280/high', '22/720x1280/low', '22/480x1440/clip', '22/master',
    ]);
    assert.equal(formats[0].label,'720×1280 · 2000 kbps');
    assert.equal(formats[0].height,1280);
    b.poster='https://pbs.twimg.com/ext_tw_video_thumb/33/img/thumb.jpg';
    const opaque = win.VDSiteMedia.inspect(b).formats;
    assert.deepEqual(Array.from(opaque, row => row.url.slice(base.length)),['33/high', '33/low', '33/opaque']);
    assert.equal(opaque[0].label,'800 kbps');
    assert.equal(opaque[0].height,undefined);
    b.poster=''; assert.equal(win.VDSiteMedia.inspect(b),null);
  } finally {win.close();}
});

test('X mediaDetails ties three videos to their own four sources and marks only the exact active URL', async () => {
  const cdn = 'https://video.twimg.com/ext_tw_video/';
  const poster = id => 'https://pbs.twimg.com/ext_tw_video_thumb/' + id + '/pu/img/preview.jpg';
  const mediaDetails = ['201','202','203'].map(id => ({
    type:'video', media_url_https:poster(id),
    video_info:{variants:[
      {content_type:'application/x-mpegURL', url:cdn + id + '/pl/master'},
      ...['320x568','480x852','720x1280'].map(size => ({
        content_type:'video/mp4', url:cdn + id + '/pu/vid/avc1/' + size + '/clip?sig=original',
      })),
    ]},
  }));
  const win = page('https://x.com/example/status/123','<article><video></video><video></video><video></video></article>',{mediaDetails});
  try {
    const [first, second, third] = win.document.querySelectorAll('video');
    [first,second,third].forEach((video,i) => {video.poster = mediaDetails[i].media_url_https;});
    await win.fetch('https://x.com/i/api/graphql/demo/TweetDetail');
    const inspect = () => win.VDSiteMedia.inspect(second);
    for (let i=0; i<40 && !inspect()?.formats.length; i++) await new Promise(r=>setTimeout(r,5));
    const currentSrc = value => Object.defineProperty(second,'currentSrc',{configurable:true, value});
    currentSrc('blob:https://x.com/opaque-player');
    const rec = inspect();
    assert.equal(rec.formats.length,4);
    assert.ok(rec.formats.every(row => row.url.startsWith(cdn + '202/') && !row.current));
    assert.notEqual(win.VDSiteMedia.inspect(first).mediaId,rec.mediaId);
    second.poster = '';
    const playing = mediaDetails[1].video_info.variants[2].url;
    currentSrc(playing);
    assert.deepEqual(Array.from(inspect().formats.filter(row => row.current),row => row.url),[playing]);
    assert.equal(inspect().mediaId,rec.mediaId);
    currentSrc(playing.replace('sig=original','sig=other'));
    assert.equal(inspect().formats.some(row => row.current),false);
    currentSrc(cdn + '202/pu/segment/0.ts');
    assert.equal(inspect().formats.length,4);
    assert.equal(inspect().formats.some(row => row.current),false);
    second.poster = poster('201');
    assert.equal(inspect(),null);
    second.poster = poster('202');
    second.src = mediaDetails[2].video_info.variants[1].url;
    assert.equal(inspect(),null);
    second.removeAttribute('src');
    second.parentElement.dataset.testid = 'placementTracking';
    assert.equal(inspect(),null);
  } finally {win.close();}
});

test('X matches opaque CDN thumbnails, rejects ambiguous or foreign ones and conflicting media groups', async () => {
  const poster = 'https://pbs.twimg.com/media/opaque.jpg';
  const direct = name => ({content_type:'video/mp4',url:'https://video.twimg.com/' + name});
  const media = [
    {type:'video', media_key:'7_41', preview_image_url:poster, variants:[direct('standalone/41')]},
    {type:'video', media_key:'7_42', preview_image_url:'https://pbs.twimg.com/media/shared', variants:[direct('standalone/42')]},
    {type:'video', media_key:'7_43', preview_image_url:'https://pbs.twimg.com/media/shared', variants:[direct('standalone/43')]},
    {type:'video', media_key:'7_44', preview_image_url:'https://pbs.twimg.com/media/broken',
      variants:[direct('ext_tw_video/44/clip.mp4'),direct('ext_tw_video/99/ad.mp4')]},
  ];
  const win = page('https://x.com/example/status/321','<article><video></video><video></video></article>',{includes:{media}});
  try {
    const video = win.document.querySelector('video');
    const inspect = () => win.VDSiteMedia.inspect(video);
    video.poster = poster + '?format=webp&name=small';
    await win.fetch('https://x.com/i/api/graphql/demo/TweetDetail');
    for (let i=0; i<40 && !inspect(); i++) await new Promise(r=>setTimeout(r,5));
    assert.equal(inspect().formats[0].url,media[0].variants[0].url);
    video.poster = poster + '?asset=other';
    assert.equal(inspect(),null);
    video.poster = 'https://elsewhere.example/ext_tw_video_thumb/41/preview.jpg';
    assert.equal(inspect(),null);
    video.poster = media[3].preview_image_url;
    assert.equal(inspect(),null);
    video.poster = media[1].preview_image_url;
    assert.equal(inspect(),null);
    Object.defineProperty(video,'currentSrc',{value:media[2].variants[0].url});
    assert.equal(inspect().formats[0].current,true);
    assert.equal(inspect().formats[0].url,media[2].variants[0].url);
    video.poster = poster;
    assert.equal(inspect(),null);
  } finally {win.close();}
});

test('X binds an opaque component thumbnail only to a matching video record', async () => {
  const pbs = 'https://pbs.twimg.com/media/';
  const media = ['one','two'].map((key, i) => ({type:'video', media_key:'7_' + (71+i),
    preview_image_url:pbs + key + '.jpg', variants:[{content_type:'video/mp4',
      url:'https://video.twimg.com/amplify_video/' + (71+i) + '/vid/640x360/clip.mp4'}]}));
  const win = page('https://x.com/example/status/123',
    '<article><img><div data-testid="videoPlayer"><div data-testid="videoComponent"><video></video><img></div></div></article>',
    {includes:{media}});
  try {
    const video = win.document.querySelector('video'), component = video.parentElement;
    const image = component.querySelector('img'), outside = win.document.querySelector('article > img');
    const inspect = () => win.VDSiteMedia.inspect(video);
    outside.src = media[1].preview_image_url;
    image.src = media[0].preview_image_url + '?name=large&format=webp';
    Object.defineProperty(video, 'currentSrc', {value:'blob:https://x.com/player'});
    assert.equal(inspect(), null);
    await win.fetch('https://api.x.com/graphql/test/TweetDetail');
    for (let i=0; i<40 && !inspect(); i++) await new Promise(r=>setTimeout(r,5));
    assert.equal(inspect()?.formats[0].url, media[0].variants[0].url);
    // Recycled elements may still carry another attachment's poster.
    video.poster = media[1].preview_image_url;
    assert.equal(inspect(), null);
    video.removeAttribute('poster');
    const overlay = win.document.createElement('div');
    overlay.style.backgroundImage = 'url("' + media[1].preview_image_url + '")';
    component.parentElement.append(overlay);
    assert.equal(inspect(), null);
    overlay.remove();
    image.src = pbs + 'unknown.jpg';
    assert.equal(inspect(), null);
    image.src = media[0].preview_image_url;
    component.append(win.document.createElement('video'));
    assert.equal(inspect(), null);
  } finally {win.close();}
});

test('X uses only the explicit canonical video attachment link around the clicked player', async () => {
  const link = 'https://twitter.com/example/status/123/video/';
  const media = ['11','22'].map((id, i) => ({media_key:'7_'+id, type:'video',
    expanded_url:link + (i+1), preview_image_url:'https://pbs.twimg.com/media/opaque'+id,
    variants:[{content_type:'video/mp4',url:'https://video.twimg.com/clip'+id}]}));
  const win = page('https://x.com/example/status/123','<article><a><video></video></a></article>',{media});
  try {
    const video = win.document.querySelector('video'), a = video.parentElement;
    const inspect = () => win.VDSiteMedia.inspect(video);
    a.href = 'https://x.com/example/status/123/video/2';
    await win.fetch('https://x.com/i/api/graphql/test/TweetDetail');
    for (let i=0; i<40 && !inspect(); i++) await new Promise(r=>setTimeout(r,5));
    assert.equal(inspect()?.formats[0].url,media[1].variants[0].url);
    video.poster = media[0].preview_image_url;
    assert.equal(inspect(),null);
    video.removeAttribute('poster');
    for (const url of ['https://x.com/example/status/123','https://wrong.test/example/status/123/video/2',
      'https://x.com/example/status/999/video/2','https://x.com/example/status/123/photo/2']) {
      a.href = url;
      assert.equal(inspect(),null);
    }
  } finally {win.close();}
});

test('Official X embed resolves only exact local video thumbnails in the matching post response', async () => {
  const pbs = 'https://pbs.twimg.com/', post = 'https://twitter.com/example/status/123';
  const endpoint = 'https://cdn.syndication.twimg.com/tweet-result?id=123';
  const url = 'https://platform.twitter.com/embed/Tweet.html?id=123';
  const entry = (id, n, type='video') => ({type, media_url_https:pbs+'media/'+id+'.jpg',
    expanded_url:post+'/video/'+n, video_info:{variants:[{content_type:'video/mp4',
      url:'https://video.twimg.com/amplify_video/'+id+'/vid/720x1280/'+id+'.mp4'}]}});
  const entries = [entry('111',1),entry('222',2,'animated_gif'),entry('333',3,'photo')];
  const quoted = {...entry('444',1),expanded_url:post.replace('123','999')+'/video/1'};
  const data = {id_str:'123',user:{screen_name:'example'}, mediaDetails:entries,quoted_tweet:{mediaDetails:[quoted]}};
  const win = page(url,'<article><a><img></a></article>',data,w => {
    // The live widget receives tweet-result with XMLHttpRequest.
    w.XMLHttpRequest.prototype.send = function () {
      Object.defineProperties(this, {responseURL:{value:endpoint},status:{value:200},
        responseType:{value:'json'},response:{value:data}});
      this.dispatchEvent(new w.Event('load'));
    };
  });
  try {
    const image = win.document.querySelector('img'), a = image.parentElement;
    a.href = post;
    const inspect = () => win.VDSiteMedia.inspect(image);
    image.src = entries[1].media_url_https + '?name=small&format=webp';
    let notifications = 0;
    win.document.addEventListener('vd-embed-media-ready-v1',()=>{notifications++;});
    for (const other of [endpoint.replace('cdn.syndication.twimg.com','x.com'),endpoint.replace('123','999'),
      endpoint.replace('tweet-result','other'),endpoint+'&id=999']) await win.fetch(other);
    await new Promise(r=>setTimeout(r,10));
    assert.equal(inspect(),null);
    assert.equal(notifications,0);
    const request = new win.XMLHttpRequest();
    request.send();
    for (let i=0; i<40 && !inspect(); i++) await new Promise(r=>setTimeout(r,5));
    assert.equal(inspect()?.formats[0].url,entries[1].video_info.variants[0].url);
    assert.equal(inspect()?.extractorUrl,'https://x.com/i/web/status/123/video/2');
    const title = inspect().title;
    assert.equal(notifications,1);
    image.src = entries[0].media_url_https;
    assert.notEqual(inspect()?.title,title);
    a.href = post.replace('123','999');
    assert.equal(inspect(),null);
    a.href = post+'/video/2';
    assert.equal(inspect(),null);
    a.href = post;
    for (const img of [quoted.media_url_https,entries[2].media_url_https,entries[0].media_url_https+'?unknown=yes',
      entries[0].media_url_https.replace('pbs.twimg.com','images.example')]) {
      image.src = img;
      assert.equal(inspect(),null);
    }
    image.src = entries[0].media_url_https;
    win.history.pushState({},'', '?id=999');
    assert.equal(inspect(),null);
  } finally {win.close();}
  // Reusing a thumbnail on multiple attachments, an ID mismatch, or the same
  // markup on a different widget path cannot identify a selected video.
  const duplicate = {...entries[1],media_url_https:entries[0].media_url_https};
  for (const [href, payload] of [[url,{...data,id_str:'999'}],[url,{...data,mediaDetails:[entries[0],duplicate]}],
    [url,{...data,mediaDetails:[entries[0],{...duplicate,type:'photo'}]}],
    [url.replace('Tweet.html','Other.html'),data],[url.replace('platform.twitter.com','x.com'),data]]) {
    const w = page(href,'<a href="'+post+'"><img src="'+entries[0].media_url_https+'"></a>',payload);
    try {
      await w.fetch(endpoint);
      await new Promise(r=>setTimeout(r,10));
      assert.equal(w.VDSiteMedia.inspect(w.document.querySelector('img')),null);
    } finally {w.close();}
  }
});

test('X exposes a scoped CDN identity before playback metadata arrives', () => {
  const id = '2111347372714328064';
  const key = 'amplify_video/' + id;
  const poster = 'https://pbs.twimg.com/amplify_video_thumb/' + id + '/img/thumb.jpg';
  const win = page('https://x.com/example/status/123', '<article><img src="https://pbs.twimg.com/ext_tw_video_thumb/77/img/post.jpg"><div data-testid="videoPlayer"><div data-testid="videoComponent"><video></video><img></div></div></article>');
  try {
    const component = win.document.querySelector('[data-testid="videoComponent"]');
    const video = component.querySelector('video'), image = component.querySelector('img');
    const inspect = () => win.VDSiteMedia.inspect(video);
    const setCurrent = value => Object.defineProperty(video,'currentSrc',{configurable:true, value});
    setCurrent('blob:https://x.com/playing');
    assert.equal(inspect(), null);
    image.src = poster + '?name=large';
    assert.equal(inspect()?.xCdnMedia, key);
    assert.equal(inspect()?.formats.length, 0);
    const other = image.cloneNode();
    other.src = poster.replace(id, '2111347372684926976');
    component.append(other);
    assert.equal(inspect(), null);
    other.remove();
    image.removeAttribute('src');
    const overlay = win.document.createElement('div');
    overlay.style.backgroundImage = 'url("' + poster + '")';
    component.parentElement.append(overlay);
    assert.equal(inspect()?.xCdnMedia, key);
    setCurrent('https://video.twimg.com/ext_tw_video/' + id + '/pl/master');
    assert.equal(inspect(), null);
    overlay.remove();
    assert.equal(inspect()?.xCdnMedia, 'ext_tw_video/' + id);
    setCurrent('blob:https://x.com/playing');
    const sibling = component.cloneNode(false);
    sibling.append(overlay);
    component.parentElement.append(sibling);
    assert.equal(inspect(), null);
    image.src = poster;
    component.append(win.document.createElement('video'));
    assert.equal(inspect(), null);
  } finally { win.close(); }
});
