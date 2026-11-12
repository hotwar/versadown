// Windows Quick CLI/MCP acceptance against an isolated host and loopback Range server.
// Usage: node scripts/verify-cli-live.cjs <results-directory> [quick-artifacts-directory]
// No existing application process, user database, account, or download is touched.
// GUI integration below is CDP/Tauri invocation, not manual desktop interaction.
// MCP coverage uses a real stdio process and a protocol test client, not an LLM app.
// VD_AGENT_RESTART_ONLY=1 runs the host-crash/restart case without the full matrix.
// VD_AGENT_UI_CHECK=1 pauses before shutdown for an external native desktop check.
const assert = require('node:assert/strict');
const crypto = require('node:crypto');
const fs = require('node:fs');
const http = require('node:http');
const net = require('node:net');
const path = require('node:path');
const { spawn, execFileSync } = require('node:child_process');

const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
const digest = bytes => crypto.createHash('sha256').update(bytes).digest('hex');
const results = [];
let root;
function terminateOwnedProcessTree(child) {
  if (!child?.pid || child.exitCode !== null || child.signalCode !== null) return;
  // Only a PID returned by this script's spawn() is accepted. On Windows,
  // terminate its own descendants too (e.g. the fixture host's aria2 process).
  // Never find or terminate processes by executable name.
  if (process.platform === 'win32') {
    try {
      execFileSync(path.join(process.env.SystemRoot || 'C:\\Windows', 'System32', 'taskkill.exe'),
        ['/PID', String(child.pid), '/T', '/F'], { windowsHide: true, stdio: 'ignore', timeout: 5000 });
      return;
    } catch { /* The process may already be exiting; retain the direct child fallback. */ }
  }
  child.kill();
}
function pass(name, detail = {}) {
  results.push({ name, result: 'PASS', ...detail });
  console.log(`PASS ${name} ${JSON.stringify(detail)}`);
}
async function until(name, fn, timeout = 30000) {
  const deadline = Date.now() + timeout;
  while (Date.now() < deadline) {
    const value = await fn();
    if (value) return value;
    await sleep(100);
  }
  throw Error(`Timed out: ${name}`);
}
async function availablePort() {
  const reservation = net.createServer();
  await new Promise((resolve, reject) => { reservation.once('error', reject); reservation.listen(0, '127.0.0.1', resolve); });
  const port = reservation.address().port;
  await new Promise(resolve => reservation.close(resolve));
  return port;
}
function run(executable, args, options = {}) {
  return new Promise((resolve, reject) => {
    const child = spawn(executable, args, { windowsHide: true, ...options });
    let stdout = '', stderr = '';
    const timer = setTimeout(() => { terminateOwnedProcessTree(child); reject(Error('Test subprocess timed out')); }, 45000);
    child.on('error', error => { clearTimeout(timer); reject(error); });
    child.stdout.on('data', chunk => { stdout += chunk; });
    child.stderr.on('data', chunk => { stderr += chunk; });
    child.on('close', code => { clearTimeout(timer); resolve({ code, stdout, stderr }); });
  });
}
async function evaluate(target, expression) {
  return new Promise((resolve, reject) => {
    const socket = new WebSocket(target.webSocketDebuggerUrl);
    const timer = setTimeout(() => { socket.close(); reject(Error('GUI evaluation timed out')); }, 15000);
    socket.addEventListener('open', () => socket.send(JSON.stringify({
      id: 1, method: 'Runtime.evaluate', params: { expression, awaitPromise: true, returnByValue: true },
    })));
    socket.addEventListener('error', () => { clearTimeout(timer); reject(Error('GUI connection failed')); });
    socket.addEventListener('message', event => {
      const value = JSON.parse(event.data);
      if (value.id !== 1) return;
      clearTimeout(timer);
      socket.close();
      if (value.error || value.result.exceptionDetails) reject(Error('GUI invocation failed'));
      else resolve(value.result.result.value);
    });
  });
}

function mcpSession(binary, env, resultDirectory, index) {
  const child = spawn(binary, ['--lang', 'en'], { env, windowsHide: true, stdio: ['pipe', 'pipe', 'pipe'] });
  const pending = new Map();
  const transcript = fs.createWriteStream(path.join(resultDirectory, `mcp-${index}.jsonl`));
  const diagnostics = fs.createWriteStream(path.join(resultDirectory, `mcp-${index}.stderr.log`));
  let buffer = '', sequence = 0, protocolError;
  const rejectPending = error => {
    protocolError = error;
    for (const waiting of pending.values()) { clearTimeout(waiting.timer); waiting.reject(error); }
    pending.clear();
  };
  child.on('error', rejectPending);
  child.stdin.on('error', rejectPending);
  child.stderr.pipe(diagnostics);
  child.stdout.setEncoding('utf8');
  child.stdout.on('data', chunk => {
    buffer += chunk;
    if (buffer.length > 8 * 1024 * 1024) { rejectPending(Error('MCP response exceeded output limit')); child.kill(); return; }
    for (;;) {
      const newline = buffer.indexOf('\n');
      if (newline < 0) break;
      const line = buffer.slice(0, newline).trim();
      buffer = buffer.slice(newline + 1);
      try {
        assert(line, 'MCP stdout must not contain blank or diagnostic lines');
        const message = JSON.parse(line);
        assert.equal(message.jsonrpc, '2.0', 'stdout must contain MCP JSON-RPC only');
        transcript.write(`${JSON.stringify({ direction: 'server', message })}\n`);
        const waiting = pending.get(message.id);
        if (!waiting) continue;
        pending.delete(message.id); clearTimeout(waiting.timer);
        if (message.error) waiting.reject(Error(`MCP ${waiting.method}: ${message.error.code}`));
        else waiting.resolve(message.result);
      } catch (error) { rejectPending(error); child.kill(); }
    }
  });
  child.on('close', (code, signal) => {
    if (buffer.trim()) rejectPending(Error('MCP left an unterminated JSON-RPC message'));
    if (pending.size) rejectPending(Error(`MCP exited during a request: ${code}/${signal}`));
    transcript.end();
  });
  function send(message) {
    if (protocolError) throw protocolError;
    assert.equal(child.exitCode, null, 'MCP process must be running');
    transcript.write(`${JSON.stringify({ direction: 'client', message })}\n`);
    child.stdin.write(`${JSON.stringify(message)}\n`);
  }
  function request(method, params = {}) {
    return new Promise((resolve, reject) => {
      const id = ++sequence;
      const timer = setTimeout(() => { pending.delete(id); reject(Error(`MCP ${method} timed out`)); }, 20000);
      pending.set(id, { resolve, reject, timer, method });
      try { send({ jsonrpc: '2.0', id, method, params }); }
      catch (error) { clearTimeout(timer); pending.delete(id); reject(error); }
    });
  }
  return {
    child,
    request,
    async initialize() {
      const result = await request('initialize', {
        protocolVersion: '2025-11-25', capabilities: {},
        clientInfo: { name: 'versadown-acceptance-protocol-client', version: '1.0.0' },
      });
      assert(result.protocolVersion && result.serverInfo, 'MCP handshake returns negotiated identity');
      send({ jsonrpc: '2.0', method: 'notifications/initialized' });
      return result;
    },
    async tool(name, arguments = {}) {
      const result = await request('tools/call', { name, arguments });
      assert(result.structuredContent && typeof result.structuredContent.ok === 'boolean');
      assert.equal(Boolean(result.isError), !result.structuredContent.ok);
      const text = result.content?.find(item => item.type === 'text');
      assert(text, 'Structured MCP tools must also provide text JSON');
      assert.deepEqual(JSON.parse(text.text), result.structuredContent);
      return result.structuredContent;
    },
    async disconnect() {
      // Simulate an Agent terminating its MCP subprocess during a download.
      terminateOwnedProcessTree(child);
      await until('owned MCP subprocess exits', () => child.exitCode !== null || child.signalCode !== null, 5000);
      if (protocolError) throw protocolError;
    },
  };
}

async function main() {
  assert(process.argv[2], 'Provide an external results directory.');
  assert.equal(process.platform, 'win32', 'This acceptance script targets Windows shells and WebView2.');
  const resultParent = path.resolve(process.argv[2]);
  fs.mkdirSync(resultParent, { recursive: true });
  root = fs.mkdtempSync(path.join(resultParent, 'cli-live-'));
  const artifacts = path.resolve(process.argv[3] || path.join(__dirname, '../artifacts/quick'));
  const cliBinary = path.join(artifacts, 'vd-cli.exe');
  const hostBinary = path.join(artifacts, 'versadown.exe');
  const mcpBinary = path.join(artifacts, 'vd-mcp-server.exe');
  const data = path.join(root, 'host');
  const directory = path.join(root, '中文 downloads');
  const discovery = path.join(data, 'cli', 'connection.json');
  fs.mkdirSync(directory, { recursive: true });
  const env = { ...process.env, VD_CLI_DISCOVERY: discovery };
  const cli = async (...args) => {
    const result = await run(cliBinary, ['--json', '--lang', 'en', ...args], { env });
    const lines = result.stdout.trim().split(/\r?\n/);
    assert.equal(lines.length, 1, 'stdout must contain one JSON result only');
    return { ...result, json: JSON.parse(lines[0]) };
  };
  const success = async (...args) => {
    const result = await cli(...args);
    assert.equal(result.code, 0, `CLI ${args[0]} failed: ${result.json.error?.code}`);
    assert.equal(result.json.ok, true);
    return result.json.data;
  };
  const failure = async (expected, exit, ...args) => {
    const result = await cli(...args);
    assert.equal(result.code, exit);
    assert.equal(result.json.error?.code, expected);
    return result;
  };
  const status = id => success('status', id);
  const waitStatus = (id, expected, timeout) => until(`task ${expected}`, async () => {
    const value = await status(id);
    return value.status === expected && value;
  }, timeout);
  const request = (command, extra = {}) => ({ command, lang: 'en', ...extra });
  const verifyInterrupt = async id => {
    const arguments = [
      '-NoProfile', '-File', path.join(__dirname, 'verify-cli-interrupt.ps1'),
      '-CliBinary', cliBinary, '-Discovery', discovery, '-TaskId', id, '-ResultDirectory', root,
    ];
    if (process.env.VD_TEST_INHERIT_CTRL_C_IGNORE === '1') arguments.push('-InheritIgnore');
    const interrupt = await run(process.env.VD_TEST_PWSH || 'powershell.exe', arguments, { env });
    assert.equal(interrupt.code, 0, `Console Ctrl-C probe failed: ${interrupt.stderr}`);
    const result = JSON.parse(interrupt.stdout);
    assert.equal(result.exit_code, 130);
    assert.equal(result.code, 'INTERRUPTED');
    assert.equal((await status(id)).status, 'paused');
    pass('real console Ctrl-C returns INTERRUPTED/130 and leaves task paused');
  };
  let connection;
  const call = async (body, headers = {}) => {
    const response = await fetch(`http://127.0.0.1:${connection.port}/v1/call`, {
      method: 'POST', headers: { 'Content-Type': 'application/json', Authorization: `Bearer ${connection.token}`, ...headers },
      body: JSON.stringify(body), signal: AbortSignal.timeout(30000),
    });
    return response.json();
  };
  const meta = {
    baseline: execFileSync('git', ['-c', `safe.directory=${path.resolve(__dirname, '..').replaceAll('\\', '/')}`, 'rev-parse', 'HEAD'], { cwd: path.resolve(__dirname, '..'), encoding: 'utf8' }).trim(),
    cliBinary, cliSha256: digest(fs.readFileSync(cliBinary)), hostBinary,
    hostSha256: digest(fs.readFileSync(hostBinary)), root, node: process.version,
    mcpBinary, mcpSha256: digest(fs.readFileSync(mcpBinary)),
    desktopEvidence: 'CDP invokes production Tauri commands; no manual desktop clicking or visual verification',
    mcpEvidence: 'real stdio MCP server plus local JSON-RPC protocol test client; no Cursor/Claude/LLM acceptance',
  };
  console.log(JSON.stringify(meta));
  fs.writeFileSync(path.join(root, 'metadata.json'), JSON.stringify(meta, null, 2));
  await success('help');
  await success('version');
  await failure('HOST_UNAVAILABLE', 3, 'health');
  await failure('INVALID_ARGUMENT', 2, 'unknown-command');
  pass('help/version/missing host/invalid arguments');

  const payload = Buffer.alloc(8 * 1024 * 1024);
  for (let index = 0; index < payload.length; index++) payload[index] = index % 251;
  const ranges = [];
  const server = http.createServer((req, res) => {
    if (req.url.startsWith('/missing')) { res.writeHead(404); res.end(); return; }
    const range = /^bytes=(\d+)-(\d*)$/.exec(req.headers.range || '');
    const start = range ? Number(range[1]) : 0;
    const end = range?.[2] ? Math.min(Number(range[2]), payload.length - 1) : payload.length - 1;
    if (start > end) { res.writeHead(416); res.end(); return; }
    ranges.push({ route: req.url, start });
    const headers = { 'Content-Type': 'application/octet-stream', 'Content-Length': end - start + 1, 'Accept-Ranges': 'bytes', ETag: '"cli-fixture-v1"' };
    if (range) headers['Content-Range'] = `bytes ${start}-${end}/${payload.length}`;
    res.writeHead(range ? 206 : 200, headers);
    if (req.method === 'HEAD') { res.end(); return; }
    if (!req.url.startsWith('/slow')) { res.end(payload.subarray(start, end + 1)); return; }
    const nativeFixture = req.url.startsWith('/slow-native');
    const restartFixture = req.url.startsWith('/slow-restart');
    const chunkSize = nativeFixture ? 8192 : restartFixture ? 16384 : 32768;
    let offset = start;
    const timer = setInterval(() => {
      const next = Math.min(offset + chunkSize, end + 1);
      res.write(payload.subarray(offset, next));
      offset = next;
      if (offset > end) { clearInterval(timer); res.end(); }
    }, nativeFixture ? 100 : restartFixture ? 50 : 40);
    res.on('close', () => clearInterval(timer));
  });
  await new Promise((resolve, reject) => { server.once('error', reject); server.listen(0, '127.0.0.1', resolve); });
  const origin = `http://127.0.0.1:${server.address().port}`;
  let host, spawnError, target, debugPort;
  let hostGeneration = 0;
  const ownedMcp = [];
  const startOwnedHost = async () => {
    spawnError = undefined;
    target = undefined;
    debugPort = await availablePort();
    const hostLogName = hostGeneration++ === 0 ? 'host.log' : `host-restart-${hostGeneration - 1}.log`;
    const hostLog = fs.openSync(path.join(root, hostLogName), 'a');
    try {
      host = spawn(hostBinary, [], {
        cwd: artifacts, windowsHide: true, stdio: ['ignore', hostLog, hostLog], env: { ...env,
          VERSADOWN_BACKGROUND_UI_TEST: '1', VERSADOWN_TEST_DATA_DIR: data,
          VERSADOWN_TEST_DOWNLOAD_DIR: directory, WEBVIEW2_USER_DATA_FOLDER: path.join(root, 'webview2'),
          WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${debugPort}`,
        },
      });
    } finally { fs.closeSync(hostLog); }
    host.on('error', error => { spawnError = error; });
    target = await until('manager GUI WebView rendered', async () => {
      if (spawnError) throw spawnError;
      assert.equal(host.exitCode, null, 'Isolated host exited during startup');
      try {
        const response = await fetch(`http://127.0.0.1:${debugPort}/json/list`);
        const values = await response.json();
        for (const candidate of values.filter(item => item.type === 'page' && item.url !== 'about:blank')) {
          if (await evaluate(candidate, 'Boolean(document.querySelector(".app-shell"))')) return candidate;
        }
        return false;
      } catch { return false; }
    });
    assert(target, 'Manager GUI must render');
  };
  try {
    await startOwnedHost();
    const invoke = (command, args = {}) => evaluate(target, `window.__TAURI_INTERNALS__.invoke(${JSON.stringify(command)}, ${JSON.stringify(args)})`);
    const initialSettings = await invoke('get_settings');
    assert.equal(initialSettings.agentAccess.enabled, false, 'Fresh profiles must default to disabled');
    assert.equal(fs.existsSync(discovery), false, 'Disabled profiles must not publish credentials');
    const configureAccess = async (enabled, allowedDirectories = [directory]) => {
      const saved = await invoke('configure_agent_access', { settings: { enabled, allowedDirectories } });
      assert.equal(saved.agentAccess.enabled, enabled);
      if (enabled) {
        await until('authorized host discovery', () => fs.existsSync(discovery));
        connection = JSON.parse(fs.readFileSync(discovery, 'utf8'));
      } else {
        assert.equal(fs.existsSync(discovery), false, 'Disabling must withdraw discovery credentials');
      }
      return saved;
    };
    await configureAccess(true);
    await until('real download engine available', async () => (await success('health')).engine_available);
    const health = await success('health');
    assert.deepEqual(health.capabilities.new_download_protocols, ['http', 'https']);
    assert.equal(health.capabilities.private_tasks, false);
    assert(health.capabilities.operations.includes('result'));
    pass('isolated host default-off authorization and real download engine health');

    const verifyHostRestart = async () => {
      const previousHealth = await success('health');
      const previousConnection = { ...connection };
      const restartArgs = ['add', `${origin}/slow-restart.bin`, '--directory', directory, '--request-id', 'host-crash-restart'];
      const task = await success(...restartArgs);
      const before = await until('restart fixture has transferred bytes and remains in flight', async () => {
        const current = await status(task.id);
        return current.status === 'downloading' && current.completed_length >= 262144
          && current.completed_length < payload.length && current;
      });
      const previousHost = host;
      const oldPid = previousHost.pid;
      // This is a crash boundary, intentionally bypassing normal exit/session
      // flushing. The helper only targets this script's host and descendants.
      terminateOwnedProcessTree(previousHost);
      await until('crashed fixture host exits', () => previousHost.exitCode !== null || previousHost.signalCode !== null, 10000);
      target = undefined;
      await startOwnedHost();
      await until('restarted host publishes new credentials', () => {
        if (!fs.existsSync(discovery)) return false;
        try {
          const candidate = JSON.parse(fs.readFileSync(discovery, 'utf8'));
          if (candidate.token === previousConnection.token) return false;
          connection = candidate;
          return true;
        } catch { return false; }
      });
      const restoredHealth = await until('restarted host engine becomes available', async () => {
        const response = await cli('health');
        return response.code === 0 && response.json.data?.engine_available && response.json.data;
      });
      assert.notEqual(restoredHealth.instance_id, previousHealth.instance_id);
      const replayed = await success(...restartArgs);
      assert.equal(replayed.id, task.id, 'Restarted idempotency replay must return the durable task');
      const replayedAgain = await success(...restartArgs);
      assert.equal(replayedAgain.id, task.id);
      const matching = (await success('list', '--query', 'slow-restart.bin', '--limit', '100')).tasks;
      assert.deepEqual(matching.map(item => item.id), [task.id], 'Restart must not create a duplicate task');
      await success('--timeout', '40', 'wait', task.id);
      const result = await success('result', task.id);
      assert.equal(result.files.length, 1);
      assert.equal(path.resolve(result.files[0].path), path.join(directory, 'slow-restart.bin'));
      assert.equal(result.files[0].exists, true);
      assert.equal(digest(fs.readFileSync(result.files[0].path)), digest(payload));
      const retainedOutputs = fs.readdirSync(directory).filter(name => name.startsWith('slow-restart.bin'));
      assert.deepEqual(retainedOutputs, ['slow-restart.bin'], 'Crash recovery must not leave duplicate outputs/checkpoints after completion');
      pass('owned host/engine crash and same-profile restart preserve task, request and file', {
        taskId: task.id, oldPid, newPid: host.pid, bytesBeforeCrash: before.completed_length,
        previousInstance: previousHealth.instance_id, restoredInstance: restoredHealth.instance_id,
        sha256: digest(payload), evidence: 'real process-tree termination and restart; no synthetic crash hook',
      });
    };

    const waitForNativeCheck = async () => {
      if (process.env.VD_AGENT_UI_CHECK !== '1') return;
      const task = await success('add', `${origin}/slow-native.bin`, '--directory', directory, '--request-id', 'native-ui-check');
      await until('native fixture has transferred bytes', async () => (await status(task.id)).completed_length > 0);
      await success('pause', task.id);
      await waitStatus(task.id, 'paused');
      const snapshot = await invoke('manager_window_snapshot');
      if (!snapshot.visible || snapshot.minimized) {
        await invoke('activate_launcher_menu_for_test', { action: 'launcher.hide_manager' });
      }
      const shown = await invoke('manager_window_snapshot');
      assert.equal(shown.visible, true, 'Native verification fixture window must be visible');
      assert.equal(shown.minimized, false);
      const readyPath = path.join(root, 'native-ready.json');
      const markerPath = path.join(root, 'native-done.marker');
      assert.equal(fs.existsSync(markerPath), false, 'Native completion marker must be created by the external check');
      fs.writeFileSync(readyPath, JSON.stringify({
        host_pid: host.pid, host_binary: hostBinary, profile: data, output_directory: directory,
        task_id: task.id, task_name: 'slow-native.bin', discovery_path: discovery, debug_port: debugPort,
        ready_at: new Date().toISOString(), completion_marker: markerPath, timeout_seconds: 180,
        evidence: 'Fixture prepared and window shown via CDP; external native interaction must be recorded separately',
      }, null, 2));
      console.log(`NATIVE_READY ${readyPath}`);
      await until('external native desktop verification marker', () => fs.existsSync(markerPath), 180000);
      const after = await status(task.id);
      fs.writeFileSync(path.join(root, 'native-after.json'), JSON.stringify({ task: after, marker_received_at: new Date().toISOString() }, null, 2));
      pass('external native verification handoff marker received', {
        taskId: task.id, statusAfter: after.status, readyPath,
        evidence: 'Marker only; external native screenshots/actions are separate evidence',
      });
    };

    const shutDownOwnedHost = async () => {
      await invoke('activate_launcher_menu_for_test', { action: 'launcher.quit' });
      await until('owned host exits', () => host.exitCode !== null, 20000);
      assert.equal(host.exitCode, 0);
      await failure('HOST_UNAVAILABLE', 3, 'health');
      pass('graceful isolated host shutdown and stale discovery');
    };

    if (process.env.VD_AGENT_RESTART_ONLY === '1') {
      await verifyHostRestart();
      await waitForNativeCheck();
      await shutDownOwnedHost();
      return;
    }
    if (process.env.VD_TEST_INTERRUPT_ONLY === '1') {
      const task = await success('add', `${origin}/slow-interrupt.bin`, '--directory', directory, '--request-id', 'interrupt-only');
      await until('interrupt fixture has transferred bytes', async () => (await status(task.id)).completed_length > 0);
      await success('pause', task.id);
      await waitStatus(task.id, 'paused');
      await verifyInterrupt(task.id);
      await invoke('activate_launcher_menu_for_test', { action: 'launcher.quit' });
      await until('interrupt-only host exits', () => host.exitCode !== null, 20000);
      assert.equal(host.exitCode, 0);
      return;
    }

    const invalidAuth = await call(request('health'), { Authorization: 'Bearer invalid-test-token' });
    assert.equal(invalidAuth.error?.code, 'UNAUTHORIZED');
    const invalidDiscovery = path.join(path.dirname(discovery), 'invalid-token.json');
    fs.writeFileSync(invalidDiscovery, JSON.stringify({ version: connection.version, port: connection.port, token: 'x'.repeat(64) }));
    await failure('UNAUTHORIZED', 4, '--discovery', invalidDiscovery, 'health');
    const crossSite = await call(request('add', { url: `${origin}/cross-site.bin`, directory, request_id: 'cross-site' }), { Origin: 'https://untrusted.invalid' });
    assert.equal(crossSite.error?.code, 'UNAUTHORIZED');
    await failure('NOT_FOUND', 5, 'status', crypto.randomUUID());
    await failure('INVALID_ARGUMENT', 2, 'add', `${origin}/invalid-directory.bin`, '--directory', 'relative-directory', '--request-id', 'invalid');
    await failure('UNSUPPORTED', 2, 'add', `${origin}/stream.m3u8`, '--directory', directory, '--request-id', 'manifest');
    pass('authentication/cross-site denial/missing task');

    const addArgs = ['add', `${origin}/slow-resume.bin`, '--directory', directory, '--request-id', 'concurrent-add'];
    const same = await Promise.all([success(...addArgs), success(...addArgs), success(...addArgs)]);
    assert(same.every(item => item.id === same[0].id));
    const first = same[0];
    const all = await success('list');
    assert.equal(all.tasks.filter(item => item.id === first.id).length, 1);
    await failure('REQUEST_CONFLICT', 6, 'add', `${origin}/different.bin`, '--directory', directory, '--request-id', 'concurrent-add');
    await failure('DUPLICATE', 6, 'add', `${origin}/slow-resume.bin`, '--directory', directory, '--request-id', 'another-request');
    pass('concurrent idempotency and request/source conflicts', { taskId: first.id });

    const guiTasks = await invoke('list_tasks');
    assert(guiTasks.some(item => item.id === first.id), 'GUI and CLI must query the same task ID');
    pass('CDP invokes GUI query and CLI sees the same task ID', { taskId: first.id });
    await until('download has transferred bytes', async () => (await status(first.id)).completed_length > 0);
    await success('pause', first.id);
    await waitStatus(first.id, 'paused');
    await failure('TASK_NOT_READY', 1, 'result', first.id);
    const paused = await status(first.id);
    await failure('TIMEOUT', 7, '--timeout', '1', 'wait', first.id);
    assert.equal((await status(first.id)).status, 'paused');
    if (process.env.VD_TEST_CTRL_C !== '0') {
      await verifyInterrupt(first.id);
    }
    await success('resume', first.id);
    await success('--timeout', '40', 'wait', first.id);
    assert.equal(digest(fs.readFileSync(path.join(directory, 'slow-resume.bin'))), digest(payload));
    assert(ranges.some(item => item.route === '/slow-resume.bin' && item.start > 0), 'Resume must issue a nonzero Range');
    pass('pause/resume/timeout leaves task unchanged/Range download hash', { bytesBeforeResume: paused.completed_length, sha256: digest(payload) });

    const completedResult = await success('result', first.id);
    assert.equal(completedResult.task_id, first.id);
    assert.equal(completedResult.status, 'completed');
    assert.equal(completedResult.files.length, 1);
    const artifact = completedResult.files[0];
    assert.equal(path.resolve(artifact.path), path.join(directory, 'slow-resume.bin'));
    assert.equal(artifact.exists, true);
    assert.equal(artifact.size_bytes, payload.length);
    assert.equal(digest(fs.readFileSync(artifact.path)), digest(payload));
    const movedArtifact = `${artifact.path}.moved-for-result-test`;
    assert.equal(fs.existsSync(movedArtifact), false);
    fs.renameSync(artifact.path, movedArtifact);
    try {
      const missingResult = await success('result', first.id);
      assert.equal(missingResult.files[0].exists, false);
      assert.equal(missingResult.files[0].path, artifact.path);
    } finally {
      fs.renameSync(movedArtifact, artifact.path);
    }
    pass('result returns verified artifact, not-ready and missing-file semantics', { sha256: digest(payload) });

    // Abort response handling after the host accepts add, then retry the same ID.
    const lostBody = request('add', { url: `${origin}/retry.bin`, directory, request_id: 'response-lost' });
    const lostResponse = await fetch(`http://127.0.0.1:${connection.port}/v1/call`, {
      method: 'POST', headers: { 'Content-Type': 'application/json', Authorization: `Bearer ${connection.token}` }, body: JSON.stringify(lostBody),
    });
    await lostResponse.body.cancel();
    const retried = await success('add', `${origin}/retry.bin`, '--directory', directory, '--request-id', 'response-lost');
    assert.equal((await success('list')).tasks.filter(item => item.id === retried.id).length, 1);
    await success('--timeout', '20', 'wait', retried.id);
    pass('discarded response and retry returns one task', { taskId: retried.id });

    const cancelled = await success('add', `${origin}/slow-cancel.bin`, '--directory', directory, '--request-id', 'cancel');
    await success('cancel', cancelled.id);
    await waitStatus(cancelled.id, 'cancelled');
    await failure('TASK_CANCELLED', 8, 'wait', cancelled.id);
    await failure('TASK_CANCELLED', 8, 'result', cancelled.id);
    const failed = await success('add', `${origin}/missing.bin`, '--directory', directory, '--request-id', 'failure');
    await failure('TASK_FAILED', 8, '--timeout', '30', 'wait', failed.id);
    await failure('TASK_FAILED', 8, 'result', failed.id);
    assert((await status(failed.id)).failure?.code, 'Async failures expose a safe machine code');
    pass('cancel state and wait terminal failures');

    const privateTask = await success('add', `${origin}/private.bin`, '--directory', directory, '--request-id', 'privacy');
    await success('--timeout', '20', 'wait', privateTask.id);
    const privatePassword = crypto.randomBytes(16).toString('hex');
    await invoke('configure_privacy_download', { enabled: true, newPassword: privatePassword });
    await invoke('unlock_privacy_tasks', { password: privatePassword });
    await invoke('set_task_private', { taskId: privateTask.id, isPrivate: true });
    assert((await invoke('list_tasks')).some(item => item.id === privateTask.id));
    assert(!(await success('list')).tasks.some(item => item.id === privateTask.id));
    for (const command of ['status', 'pause', 'resume', 'cancel', 'result']) await failure('NOT_FOUND', 5, command, privateTask.id);
    await failure('NOT_FOUND', 5, 'list', '--parent-id', privateTask.id);
    // Even a successful idempotency replay must not expose a now-private task.
    await failure('NOT_FOUND', 5, 'add', `${origin}/private.bin`, '--directory', directory, '--request-id', 'privacy');
    pass('unlocked GUI privacy stays inaccessible to CLI');

    const expectedTasks = (await success('list', '--limit', '100')).tasks.map(task => task.id).sort();
    const pagedIds = [];
    let cursor;
    do {
      const page = await success('list', '--limit', '2', ...(cursor ? ['--cursor', cursor] : []));
      assert(page.tasks.length <= 2);
      pagedIds.push(...page.tasks.map(task => task.id));
      assert(page.tasks.every(task => !('children' in task)), 'Task projection must not recursively expand children');
      cursor = page.next_cursor;
    } while (cursor);
    assert.equal(new Set(pagedIds).size, pagedIds.length);
    assert.deepEqual(pagedIds.sort(), expectedTasks);
    assert.deepEqual((await success('list', '--parent-id', first.id)).tasks, []);
    const firstPage = await success('list', '--limit', '1');
    assert(firstPage.next_cursor);
    await failure('INVALID_ARGUMENT', 2, 'list', '--cursor', firstPage.next_cursor, '--status', 'completed');
    await failure('INVALID_ARGUMENT', 2, 'list', '--limit', '101');
    pass('pagination has no duplicates and private parent lookup stays inaccessible', { visibleTasks: expectedTasks.length });

    const restrictedDirectory = path.join(directory, 'restricted');
    const outsideDirectory = path.join(root, 'outside-authorized-root');
    fs.mkdirSync(restrictedDirectory);
    fs.mkdirSync(outsideDirectory);
    await failure('FORBIDDEN', 4, 'add', `${origin}/outside.bin`, '--directory', outsideDirectory, '--request-id', 'outside-root');
    const permissionTask = await success('add', `${origin}/slow-permission.bin`, '--directory', directory, '--request-id', 'permission-resume');
    await until('permission task has transferred bytes', async () => (await status(permissionTask.id)).completed_length > 0);
    await success('pause', permissionTask.id);
    await waitStatus(permissionTask.id, 'paused');
    await configureAccess(true, [restrictedDirectory]);
    await failure('FORBIDDEN', 4, 'resume', permissionTask.id);
    assert.equal((await status(permissionTask.id)).status, 'paused');
    await failure('FORBIDDEN', 4, 'result', first.id);
    await configureAccess(true);
    await success('cancel', permissionTask.id);
    assert.equal(digest(fs.readFileSync(artifact.path)), digest(payload), 'Permission changes must preserve existing completed files');
    pass('new downloads, existing resume and results obey updated directory grants');

    const continuing = await success('add', `${origin}/slow-revoke.bin`, '--directory', directory, '--request-id', 'revoke-access');
    await until('revocation fixture is running', async () => (await status(continuing.id)).completed_length > 0);
    const revoked = { ...connection };
    await configureAccess(false);
    await failure('HOST_UNAVAILABLE', 3, 'health');
    let denied = false;
    try {
      const oldResponse = await fetch(`http://127.0.0.1:${revoked.port}/v1/call`, {
        method: 'POST', headers: { 'Content-Type': 'application/json', Authorization: `Bearer ${revoked.token}` },
        body: JSON.stringify(request('health')), signal: AbortSignal.timeout(3000),
      });
      const oldPayload = await oldResponse.json();
      denied = !oldPayload.ok && ['UNAUTHORIZED', 'HOST_DISABLED'].includes(oldPayload.error?.code);
    } catch (error) {
      // A closed listener rejects transport access; a timeout alone is not proof of revocation.
      denied = ['ECONNREFUSED', 'ECONNRESET', 'UND_ERR_SOCKET'].includes(error?.cause?.code);
    }
    assert(denied, 'The revoked endpoint/token must no longer authorize calls');
    await until('download continues while Agent access is disabled', async () => {
      const tasks = await invoke('list_tasks');
      return tasks.some(task => task.id === continuing.id && task.status === 'completed');
    }, 40000);
    await configureAccess(true);
    assert.notEqual(connection.token, revoked.token);
    const oldTokenOnNewEndpoint = await call(request('health'), { Authorization: `Bearer ${revoked.token}` });
    assert.equal(oldTokenOnNewEndpoint.error?.code, 'UNAUTHORIZED');
    await success('--timeout', '20', 'wait', continuing.id);
    assert.equal(digest(fs.readFileSync(path.join(directory, 'slow-revoke.bin'))), digest(payload));
    pass('disabling revokes credentials and preserves in-flight downloads; re-enable rotates token');

    const mcp = mcpSession(mcpBinary, env, root, 1); ownedMcp.push(mcp);
    const initialized = await mcp.initialize();
    const definitions = await mcp.request('tools/list');
    const toolNames = definitions.tools.map(tool => tool.name);
    for (const name of ['vd_health', 'vd_download_add', 'vd_task_list', 'vd_task_status', 'vd_task_pause', 'vd_task_resume', 'vd_task_cancel', 'vd_task_result']) {
      const tool = definitions.tools.find(tool => tool.name === name);
      assert(tool?.inputSchema && tool.outputSchema, `MCP ${name} must publish both schemas`);
    }
    assert.equal(toolNames.length, 8);
    assert.equal((await mcp.tool('vd_health')).ok, true);
    const mcpArguments = { url: `${origin}/slow-mcp.bin`, directory, request_id: 'mcp-stdio-download' };
    const mcpAdded = await mcp.tool('vd_download_add', mcpArguments);
    assert.equal(mcpAdded.ok, true);
    const mcpTaskId = mcpAdded.data.id;
    assert.equal((await mcp.tool('vd_download_add', mcpArguments)).data.id, mcpTaskId);
    assert.equal((await mcp.tool('vd_task_status', { task_id: mcpTaskId })).data.id, mcpTaskId);
    const invalidToolResult = await mcp.tool('vd_task_status', { task_id: crypto.randomUUID() });
    assert.equal(invalidToolResult.error?.code, 'NOT_FOUND');
    await until('MCP download is in flight', async () => {
      const task = await status(mcpTaskId);
      return task.completed_length > 0 && task.status === 'downloading';
    });
    await mcp.disconnect();
    await success('--timeout', '40', 'wait', mcpTaskId);
    const reconnected = mcpSession(mcpBinary, env, root, 2); ownedMcp.push(reconnected);
    await reconnected.initialize();
    assert.equal((await reconnected.tool('vd_task_status', { task_id: mcpTaskId })).data.status, 'completed');
    const mcpResult = await reconnected.tool('vd_task_result', { task_id: mcpTaskId });
    assert.equal(mcpResult.ok, true);
    assert.equal(mcpResult.data.files.length, 1);
    assert.equal(mcpResult.data.files[0].exists, true);
    assert.equal(path.resolve(mcpResult.data.files[0].path), path.join(directory, 'slow-mcp.bin'));
    assert.equal(digest(fs.readFileSync(mcpResult.data.files[0].path)), digest(payload));
    await reconnected.disconnect();
    pass('real MCP stdio discovery/download/status/result and process disconnect persistence', {
      protocolVersion: initialized.protocolVersion, serverInfo: initialized.serverInfo, taskId: mcpTaskId,
      sha256: digest(payload), client: 'JSON-RPC acceptance client; not an LLM desktop client',
    });

    await verifyHostRestart();

    // Use the real shell parsers, including a Chinese directory and spaces.
    const ps = process.env.VD_TEST_PWSH || 'powershell.exe';
    const psQuote = value => "'" + value.replaceAll("'", "''") + "'";
    const psArgs = ['--json', 'add', `${origin}/powershell.bin`, '--directory', directory, '--request-id', 'powershell'];
    const psResult = await run(ps, ['-NoProfile', '-Command', `& ${psQuote(cliBinary)} ${psArgs.map(psQuote).join(' ')}; exit $LASTEXITCODE`], { env });
    assert.equal(psResult.code, 0); assert.equal(JSON.parse(psResult.stdout).ok, true);
    const cmdResult = await run(process.env.ComSpec || 'cmd.exe', ['/d', '/s', '/c', `""${cliBinary}" --json add "${origin}/cmd.bin" --directory "${directory}" --request-id cmd"`], { env, windowsVerbatimArguments: true });
    assert.equal(cmdResult.code, 0); assert.equal(JSON.parse(cmdResult.stdout).ok, true);
    const bash = process.env.VD_TEST_BASH || path.join(process.env.ProgramFiles || 'C:\\Program Files', 'Git/bin/bash.exe');
    const bashQuote = value => "'" + value.replaceAll("'", "'\\''") + "'";
    const bashArgs = [cliBinary.replaceAll('\\', '/'), '--json', 'add', `${origin}/git-bash.bin`, '--directory', directory.replaceAll('\\', '/'), '--request-id', 'git-bash'];
    const bashResult = await run(bash, ['-lc', bashArgs.map(bashQuote).join(' ')], { env });
    assert.equal(bashResult.code, 0); assert.equal(JSON.parse(bashResult.stdout).ok, true);
    for (const [shell, command, args] of [
      ['PowerShell', ps, ['-NoProfile', '-Command', `& ${psQuote(cliBinary)} --json unknown; exit $LASTEXITCODE`]],
      ['cmd', process.env.ComSpec || 'cmd.exe', ['/d', '/s', '/c', `""${cliBinary}" --json unknown"`]],
      ['Git Bash', bash, ['-lc', `${bashQuote(cliBinary.replaceAll('\\', '/'))} --json unknown`]],
    ]) {
      const result = await run(command, args, { env, windowsVerbatimArguments: shell === 'cmd' });
      assert.equal(result.code, 2); assert.equal(JSON.parse(result.stdout).error.code, 'INVALID_ARGUMENT');
    }
    pass('PowerShell/cmd/Git Bash Chinese paths, JSON and nonzero exits');
    await waitForNativeCheck();
    await shutDownOwnedHost();
  } finally {
    for (const session of ownedMcp) {
      terminateOwnedProcessTree(session.child);
    }
    if (host?.exitCode === null && host.pid && target) {
      try {
        await evaluate(target, 'window.__TAURI_INTERNALS__.invoke("activate_launcher_menu_for_test", { action: "launcher.quit" })');
      } catch { /* The WebView can close before replying to the quit command. */ }
      try {
        await until('owned host cleanup', () => host.exitCode !== null || host.signalCode !== null, 5000);
      } catch { /* A broken isolated host is terminated below; no other instance is touched. */ }
    }
    terminateOwnedProcessTree(host);
    server.closeAllConnections();
    await new Promise(resolve => server.close(resolve));
  }
  if (process.env.VD_TEST_CTRL_C === '0') console.log('NOT_RUN: real console Ctrl-C probe explicitly disabled.');
}
main().catch(error => {
  results.push({ name: 'acceptance', result: 'FAIL', detail: error.message });
  console.error(`FAIL ${error.stack}`);
  process.exitCode = 1;
}).finally(() => {
  const summary = { passed: results.filter(item => item.result === 'PASS').length, failed: results.filter(item => item.result === 'FAIL').length, results };
  if (root) fs.writeFileSync(path.join(root, 'summary.json'), JSON.stringify(summary, null, 2));
  console.log(`SUMMARY ${JSON.stringify(summary)}`);
});
