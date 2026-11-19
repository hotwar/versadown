// Register the browser-enabled VD executable as a per-user Chrome native host.
import { access, mkdir, readFile, realpath, rename, writeFile } from 'node:fs/promises';
import { constants } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { homedir, platform } from 'node:os';
import { execFileSync } from 'node:child_process';
import { createHash, randomUUID } from 'node:crypto';
import { parseArgs } from 'node:util';

const { values: { app, 'manifest-dir': customDir } } = parseArgs({ options: {
  app: {type: 'string'}, 'manifest-dir': {type: 'string'}
} });
if (!app) throw new Error('Usage: node scripts/register-chrome-host.mjs --app <browser-enabled VersaDown executable or .app> [--manifest-dir <test directory>]');
const os = platform();
const exe = await realpath(os === 'darwin' && /\.app\/?$/.test(app) ? join(app, 'Contents/MacOS/versadown') : app);
await access(exe, constants.X_OK);
const extension = JSON.parse(await readFile(new URL('../browser-extension/manifest.json', import.meta.url), 'utf8'));
const id = [...createHash('sha256').update(Buffer.from(extension.key, 'base64')).digest('hex').slice(0,32)]
  .map(d => String.fromCharCode(97 + parseInt(d,16))).join('');
const host = 'com.versadown.browser';
const root = customDir || ({
  darwin: join(homedir(), 'Library/Application Support/Google/Chrome/NativeMessagingHosts'),
  linux: join(process.env.XDG_CONFIG_HOME || join(homedir(), '.config'), 'google-chrome/NativeMessagingHosts'),
  win32: join(process.env.LOCALAPPDATA || homedir(), 'VersaDown', 'NativeMessagingHosts'),
})[os];
if (!root) throw new Error('Unsupported OS');
const manifest = resolve(root, host + '.json');
const body = {name:host, description:'VersaDown browser media', path:exe, type:'stdio', allowed_origins:['chrome-extension://' + id + '/']};
await mkdir(dirname(manifest), {recursive:true});
const tmp = manifest + '.' + randomUUID() + '.tmp';
await writeFile(tmp, JSON.stringify(body, null, 2) + '\n', {mode:0o600, flag:'wx'});
await rename(tmp, manifest);
if (os === 'win32' && !customDir) {
  execFileSync('reg.exe', ['add', 'HKCU\\Software\\Google\\Chrome\\NativeMessagingHosts\\' + host, '/ve', '/t', 'REG_SZ', '/d', manifest, '/f']);
}
console.log(manifest);
