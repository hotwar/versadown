import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';

const file = name => readFileSync(new URL('../' + name, import.meta.url), 'utf8');
const manifest = JSON.parse(file('manifest.json'));
const plain = value => JSON.parse(JSON.stringify(value));

function worker() {
  const event = () => ({
    listeners: [],
    addListener(listener) { this.listeners.push(listener); },
    fire(...args) { for (const listener of this.listeners) listener(...args); },
  });
  const local = {};
  const registered = new Map();
  const executions = [];
  const chrome = {
    runtime: {
      getManifest: () => manifest,
      getURL: name => 'chrome-extension://fixture/' + name,
      onStartup: event(),
      onInstalled: event(),
      onMessage: event(),
    },
    storage: {
      local: {
        get: async () => ({...local}),
        set: async values => {
          Object.assign(local, values);
          chrome.storage.onChanged.fire(values, 'local');
        },
      },
      onChanged: event(),
    },
    permissions: {contains: async () => false, onRemoved: event()},
    scripting:{
      getRegisteredContentScripts: async ({ids}) => ids.map(id => registered.get(id)).filter(Boolean),
      registerContentScripts: async scripts => {
        for (const script of scripts) {
          assert(!registered.has(script.id));
          registered.set(script.id, plain(script));
        }
      },
      unregisterContentScripts: async ({ids}) => { for (const id of ids) registered.delete(id); },
      executeScript: async args => { executions.push(plain(args)); },
    },
    tabs: {query: async () => [{id:1, url:'https://video.example/'}]},
  };
  const context = vm.createContext({chrome, URL});
  for (const s of ['detection-scope.js','detection.js']) vm.runInContext(file(s),context);
  const send = () => new Promise(resolve => chrome.runtime.onMessage.fire(
    {action:'refreshVideoDetection'}, {url:chrome.runtime.getURL('popup.html')}, resolve));
  return {chrome, scope:context.VDDetection, local, registered, executions, send};
}

test('built-in scope includes X and excludes other video pages', () => {
  const {scope} = worker();
  for (const url of [
    'https://www.bilibili.com/video/BV1ps411o7yC/',
    'https://www.youtube.com/watch?v=xxxx',
    'https://x.com/user/status/10',
    'https://platform.twitter.com/embed/Tweet.html?id=10',
    'http://127.0.0.1:9823/test',
  ]) assert(scope.isBuiltInPage(url), url);
  for (const url of [
    'https://x.com.example/',
    'https://platform.twitter.com/other',
    'https://video.example/',
    'chrome://extensions/',
  ]) assert(!scope.isBuiltInPage(url), url);
});

test('detect-all is off by default, then installs an opted-in script without duplicating built-in overlays', async () => {
  const {chrome, scope, registered, executions, send} = worker();
  assert.deepEqual(plain(await send()), {ok:true, enabled:false});
  assert.equal(registered.size, 0);
  assert.equal(executions.length, 0);
  chrome.permissions.contains = async opts => {
    assert.deepEqual(plain(opts.origins), manifest.optional_host_permissions);
    return true;
  };
  await chrome.storage.local.set({[scope.setting]:true});
  assert.deepEqual(plain(await send()), {ok:true, enabled:true});
  const [script] = registered.values();
  assert.deepEqual(script.matches, manifest.optional_host_permissions);
  assert.deepEqual(script.excludeMatches, plain(scope.builtInMatches));
  assert(script.allFrames && script.persistAcrossSessions);
  assert.deepEqual(executions, [{target:{tabId:1,allFrames:true},files:plain(scope.files)}]);
  await chrome.storage.local.set({[scope.setting]:false});
  assert.deepEqual(plain(await send()), {ok:true, enabled:false});
  assert.equal(registered.size, 0);
});

test('revoking broad site grant also disables the detection preference', async () => {
  const {chrome, scope, local, registered, send} = worker();
  chrome.permissions.contains = async () => true;
  await chrome.storage.local.set({[scope.setting]:true});
  await send();
  assert.equal(registered.size, 1);
  chrome.permissions.contains = async () => false;
  chrome.permissions.onRemoved.fire();
  await send();
  assert.equal(local[scope.setting], false);
  assert.equal(registered.size, 0);
});
