import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import test from 'node:test';
import {JSDOM} from 'jsdom';

const script = file => readFileSync(new URL('../' + file, import.meta.url), 'utf8');
function surface(url, body, inspect, send) {
  const {window} = new JSDOM(body,{url,runScripts:'outside-only',pretendToBeVisual:true});
  const roots = new Map(), clicks = new Map();
  const shadow = window.Element.prototype.attachShadow;
  window.Element.prototype.attachShadow = function(options) {
    const root = shadow.call(this,options);
    roots.set(this,root);
    return root;
  };
  const listen = window.HTMLButtonElement.prototype.addEventListener;
  window.HTMLButtonElement.prototype.addEventListener = function(type,handler,...rest) {
    if (type === 'click') clicks.set(this,handler);
    return listen.call(this,type,handler,...rest);
  };
  for (const el of window.document.querySelectorAll('video,img,[style*="background-image"]')) {
    el.getBoundingClientRect = () => ({left:120,top:30,right:420,bottom:250,height:220,width:300});
    el.checkVisibility = () => el.style.opacity !== '0';
  }
  window.ResizeObserver = class { observe() {} unobserve() {} };
  window.VDSiteMedia = {inspect};
  window.chrome = {i18n:{getMessage:key => key},runtime:{sendMessage:send}};
  window.eval(script('media.js'));
  window.eval(script('content.js'));
  const controls = () => [...roots].filter(([host])=>host.isConnected).map(([,root])=>root);
  const click = node => clicks.get(node)({isTrusted:true,stopPropagation(){},preventDefault(){}});
  return {window,controls,click};
}

test('late matching metadata enables only its X embed thumbnail and a source click submits its attachment', async () => {
  let available = false;
  const messages = [];
  const {window,controls,click} = surface('https://platform.twitter.com/embed/Tweet.html?id=987',
    '<img id="avatar" src="/photo"><img id="clip" src="/thumb">', el =>
      available && el.id === 'clip' && el.src.endsWith('/thumb') ? {
        mediaId:'cdn:ext_tw_video/1',extractorUrl:'https://x.com/i/web/status/987/video/2',
        formats:[{kind:'direct',url:'https://video.twimg.com/film.mp4'}]
      } : null, async m => {
        messages.push(m);
        return m.action === 'resolveVideo' ? {ok:true,options:[{handle:'selected',label:'720×1280'}]} : {ok:true};
      });
  try {
    assert.equal(controls().length,0);
    available = true;
    window.document.dispatchEvent(new window.Event('vd-embed-media-ready-v1'));
    await new Promise(resolve => window.requestAnimationFrame(resolve));
    assert.equal(controls().length,1);
    const root = controls()[0];
    await click(root.querySelector('button'));
    assert.deepEqual(messages.map(m=>m.action),['resolveVideo']);
    assert.equal(messages[0].targets[0].url,'https://x.com/i/web/status/987/video/2');
    assert.equal(root.querySelector('.choices button').textContent,'720×1280');
    await click(root.querySelector('.choices button'));
    assert.equal(messages[1].action,'downloadVideo');
    assert.equal(messages[1].handle,'selected');
    window.document.querySelector('#clip').src='/ordinary-image';
    await Promise.resolve();
    assert.equal(controls().length,0);
  } finally {window.close();}
});

test('on a status page photos have no controls, and only a matching post offers an explicit embed link', async () => {
  for (const [route,owner,expected] of [['/maker/status/123','/maker/status/123',true],
    ['/maker/status/123','/reply/status/456',false],['/home','/maker/status/123',false]]) {
    const messages=[];
    const {window,controls,click} = surface('https://x.com'+route,
      '<article><a href="'+owner+'"><time>Today</time></a><video></video><img src="/photo"></article>',
      () => null,async m => {messages.push(m);return {ok:false,reason:'noMedia'};});
    try {
      assert.equal(controls().length,1);
      const root = controls()[0];
      await click(root.querySelector('button'));
      const link = root.querySelector('output a');
      assert.equal(Boolean(link),expected);
      if (link) {
        assert.equal(link.href,'https://platform.twitter.com/embed/Tweet.html?id=123');
        assert.equal(link.target,'_blank');
        assert.equal(link.textContent,'openPublicEmbed');
      }
      assert.deepEqual(messages,[]);
    } finally {window.close();}
  }
});

test('a transparent accessibility thumbnail uses only the same visible poster inside its own attachment link', () => {
  const source = 'https://pbs.twimg.com/amplify_video_thumb/456/img/still.jpg';
  const {window,controls} = surface('https://platform.twitter.com/embed/Tweet.html?id=123',
    '<a href="/post"><div style="background-image:url('+source+')"></div><img src="'+source+'" style="opacity:0"></a>' +
    '<a href="/other"><div style="background-image:url(https://pbs.twimg.com/other.jpg)"></div><img src="'+source+'" style="opacity:0"></a>',
    () => ({mediaId:'cdn:amplify_video/456',formats:[{kind:'hls'}]}),async()=>{throw Error('unexpected handoff');});
  try { assert.equal(controls().length,1); } finally {window.close();}
});
