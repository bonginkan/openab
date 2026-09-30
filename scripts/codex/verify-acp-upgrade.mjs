import { spawn, execFileSync } from 'node:child_process';
import { createInterface } from 'node:readline';
import { readFileSync, writeFileSync } from 'node:fs';
import { randomUUID } from 'node:crypto';
import { homedir } from 'node:os';
import { isAbsolute } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';

// Usage: node verify-acp-upgrade.mjs SPEC.json STATE.json new [RED.png]
//        node verify-acp-upgrade.mjs SPEC.json STATE.json resume
// Keep host specs and state outside the repository. SPEC contains the absolute
// command and working_dir, string-array args, and the model to verify. No secrets
// or extra environment variables are accepted. The probe uses the existing login.
const [specPath, statePath, mode = 'new', imagePath] = process.argv.slice(2);
if (!specPath || !statePath || !['new', 'resume'].includes(mode)) throw new Error('Expected spec, state path, and new/resume mode');
const config = JSON.parse(readFileSync(specPath, 'utf8'));
if (typeof config.command !== 'string' || !isAbsolute(config.command)
    || typeof config.working_dir !== 'string' || !isAbsolute(config.working_dir)
    || !Array.isArray(config.args) || !config.args.every(value => typeof value === 'string')
    || typeof config.model !== 'string' || !config.model || config.env !== undefined) {
  throw new Error('Invalid probe spec');
}
const args = [...config.args, '-c', 'model=' + JSON.stringify(config.model)];
const env = { HOME: homedir(), PATH: process.env.PATH };
for (const key of process.platform === 'win32' ? ['USERPROFILE', 'USERNAME', 'SystemRoot', 'SystemDrive'] : ['USER']) {
  if (process.env[key] !== undefined) env[key] = process.env[key];
}
const child = spawn(config.command, args, {
  cwd: config.working_dir,
  detached: true,
  env,
  stdio: ['pipe', 'pipe', 'pipe'],
});
const pending = new Map();
const errorKinds = new Set();
const toolStatuses = [];
let nextId = 1;
let output = '';
let sessionId;
let steering;
let startedTool = false;
let primaryPending = false;
let steeredInFlight = false;
let finalReport;
let finalState;
const send = message => child.stdin.write(JSON.stringify(message) + '\n');
function call(method, params, captureCompletion = false) {
  const id = nextId++;
  return new Promise((resolve, reject) => {
    pending.set(id, { resolve, reject, captureCompletion });
    send({ jsonrpc: '2.0', id, method, params });
  });
}
const failPending = error => {
  for (const request of pending.values()) request.reject(error);
  pending.clear();
};
child.on('error', failPending);
child.on('exit', (code, signal) => failPending(new Error('ACP exited: ' + (signal ?? code))));
child.stdin.on('error', failPending);
child.stderr.on('data', data => {
  const text = data.toString();
  if (/base_instructions/.test(text)) errorKinds.add('model_catalog_error');
  if (/code.mode.host/.test(text) && /No such file|not found|failed to spawn/.test(text)) errorKinds.add('sidecar_missing');
  if (/not supported when using Codex with a ChatGPT account/.test(text)) errorKinds.add('model_unsupported');
});
createInterface({ input: child.stdout }).on('line', line => {
  let message;
  try { message = JSON.parse(line); } catch { return; }
  if (message.id !== undefined && !message.method) {
    const request = pending.get(message.id);
    if (!request) return;
    pending.delete(message.id);
    if (request.captureCompletion) primaryPending = false;
    if (message.error) request.reject(new Error('ACP error code ' + message.error.code));
    else request.resolve(request.captureCompletion ? { result: message.result, outputAtCompletion: output } : message.result);
  } else if (message.method === 'session/update') {
    const update = message.params?.update;
    if (update?.sessionUpdate === 'agent_message_chunk' && update.content?.type === 'text') output += update.content.text;
    if (update?.sessionUpdate === 'tool_call_update') toolStatuses.push(update.status);
    if (mode === 'new' && primaryPending && !startedTool
        && ['tool_call', 'tool_call_update'].includes(update?.sessionUpdate)
        && !['completed', 'failed'].includes(update.status)) {
      startedTool = true;
      steeredInFlight = true;
      steering = call('session/prompt', { sessionId, prompt: [{ type: 'text', text: 'This is an authorized in-flight steering probe. Retain the continuation token, finish the existing tool operation, and add MISA_STEER_OK to your final answer. Do not start another tool.' }] });
      steering.catch(() => {});
    }
  } else if (message.id !== undefined) {
    send({ jsonrpc: '2.0', id: message.id, error: { code: -32601, message: 'Unsupported client method in bounded probe' } });
  }
});
// Observe ancestry while ACP is alive so cleanup also knows detached descendants.
// Freeze parents before the final discovery pass; do not permit TERM handlers to
// fork between enumeration and termination. Identity checks prevent PID reuse.
const owned = new Map();
const groups = new Set();
let observationError;
function processes() {
  return execFileSync('ps', ['-axo', 'pid=,ppid=,pgid=,stat=,lstart=,comm='], { encoding: 'utf8', timeout: 2000 })
    .trim().split('\n').map(line => {
      const match = line.trim().match(/^(\d+)\s+(\d+)\s+(\d+)\s+(\S+)\s+(\w+\s+\w+\s+\d+\s+\d+:\d+:\d+\s+\d+)\s+(.+)$/);
      if (!match) throw new Error('Unrecognized process metadata');
      return { pid: +match[1], ppid: +match[2], pgid: +match[3], state: match[4], identity: match[5] + ' ' + match[6] };
    });
}
function discover() {
  const rows = processes();
  if (!owned.size) {
    const root = rows.find(row => row.pid === child.pid);
    if (root) owned.set(root.pid, root);
  }
  for (const group of groups) if (!rows.some(row => row.pgid === group)) groups.delete(group);
  let changed;
  do {
    changed = false;
    const live = new Set(rows.filter(row => owned.get(row.pid)?.identity === row.identity).map(row => row.pid));
    for (const row of rows) {
      if (live.has(row.pid) && row.pid === row.pgid) groups.add(row.pgid);
      if (!live.has(row.pid) && (live.has(row.ppid) || groups.has(row.pgid))) {
        if (row.pid === process.pid || row.pid === process.ppid) throw new Error('Probe observer reached an unrelated owner');
        owned.set(row.pid, row);
        changed = true;
      }
    }
  } while (changed);
  return rows.filter(row => owned.get(row.pid)?.identity === row.identity && !row.state.includes('Z'));
}
function signal(row, name) {
  if (!processes().some(current => current.pid === row.pid && current.identity === row.identity)) return;
  try { process.kill(row.pid, name); } catch (error) { if (error.code !== 'ESRCH') throw error; }
}
function observe() { try { discover(); } catch (error) { observationError = error; } }
if (process.platform !== 'win32') child.once('spawn', observe);
const observer = process.platform === 'win32' ? null : setInterval(observe, 200);
async function cleanup() {
  clearInterval(observer);
  if (!child.pid) return;
  if (process.platform === 'win32') {
    if (child.exitCode === null && child.signalCode === null) {
      execFileSync('taskkill', ['/PID', String(child.pid), '/T', '/F'], { stdio: 'ignore', timeout: 5000 });
    } else {
      throw new Error('ACP exited before Windows tree cleanup; descendants cannot be verified');
    }
    return;
  }
  const deadline = Date.now() + 3000;
  let stable = 0;
  while (Date.now() < deadline) {
    const before = owned.size;
    const rows = discover();
    for (const row of rows) if (!row.state.includes('T')) signal(row, 'SIGSTOP');
    stable = before === owned.size && rows.every(row => row.state.includes('T')) ? stable + 1 : 0;
    if (stable >= 2) break;
    await delay(25);
  }
  for (const row of discover()) signal(row, 'SIGKILL');
  const killDeadline = Date.now() + 2000;
  while (Date.now() < killDeadline && discover().length) await delay(25);
  if (discover().length) throw new Error('Probe descendants remain after cleanup');
  if (stable < 2 || observationError) throw new Error('Probe process-tree observation was incomplete');
}
let timedOut = false;
const timeout = setTimeout(() => {
  timedOut = true;
  failPending(new Error('ACP_PROBE_TIMEOUT'));
}, 180000);
try {
  await call('initialize', { protocolVersion: 1, clientCapabilities: {}, clientInfo: { name: 'misa-model-update-probe', version: '1' } });
  const state = mode === 'resume' ? JSON.parse(readFileSync(statePath, 'utf8')) : { token: 'MISA_MEMORY_' + randomUUID().replaceAll('-', '') };
  const session = mode === 'resume'
    ? { ...(await call('session/load', { sessionId: state.sessionId, cwd: config.working_dir, mcpServers: [] })), sessionId: state.sessionId }
    : await call('session/new', { cwd: config.working_dir, mcpServers: [] });
  sessionId = session.sessionId;
  console.log(JSON.stringify({ event: 'session', mode, sessionId, pid: child.pid, currentModelId: session.models?.currentModelId }));
  const settings = await call('session/set_config_option', { sessionId, configId: 'model', value: config.model });
  const selected = settings.configOptions?.filter(option => ['model', 'reasoning_effort'].includes(option.id)).map(option => ({ id: option.id, currentValue: option.currentValue }));
  console.log(JSON.stringify({ event: 'settings', selected }));
  if (!selected?.some(option => option.id === 'model' && option.currentValue === config.model)) {
    throw new Error('ACP did not confirm the requested model');
  }
  output = '';
  toolStatuses.length = 0;
  const prompt = mode === 'resume'
    ? 'This is the authorized session/load continuity check. Return only the exact MISA_MEMORY continuation token you were asked to remember in the preceding turn. Do not use tools.'
    : 'This is a bounded authorized operational probe. Remember the continuation token ' + state.token + '. Call exec_command exactly once with: node -e \'setTimeout(()=>console.log("MISA_TOOL_OK"),8000)\'. Do not read files or credentials, edit files, send messages, or do other work. Then return MISA_TOOL_OK and the continuation token. A follow-up instruction may arrive during the tool.';
  const content = [{ type: 'text', text: prompt }];
  if (mode === 'new' && imagePath) {
    content[0].text += ' Identify the dominant color of the attached image and include MISA_IMAGE_<COLOR> using the uppercase English color name in your final response.';
    content.push({ type: 'image', mimeType: 'image/png', data: readFileSync(imagePath).toString('base64') });
  }
  primaryPending = true;
  const { result, outputAtCompletion } = await call('session/prompt', { sessionId, prompt: content }, true);
  const steerResult = steering ? await steering : null;
  const passed = !timedOut && result.stopReason === 'end_turn' && outputAtCompletion.includes(state.token) && errorKinds.size === 0
    && (mode === 'resume' || (steeredInFlight && toolStatuses.includes('completed') && outputAtCompletion.includes('MISA_TOOL_OK') && outputAtCompletion.includes('MISA_STEER_OK') && steerResult?.stopReason === 'end_turn' && (!imagePath || outputAtCompletion.includes('MISA_IMAGE_RED'))));
  finalReport = { event: 'result', mode, sessionId, requestedModel: config.model, selected, stopReason: result.stopReason, steerStopReason: steerResult?.stopReason, steeredInFlight, output: outputAtCompletion, toolStatuses, errorKinds: [...errorKinds], passed };
  finalState = state;
  if (!passed) process.exitCode = 1;
} catch (error) {
  console.error(error.message);
  process.exitCode = 1;
} finally {
  clearTimeout(timeout);
  const cleanupDeadline = setTimeout(() => {
    console.error('ACP_PROBE_CLEANUP_TIMEOUT');
    process.exit(1);
  }, 8000);
  try {
    await cleanup();
  } catch (error) {
    console.error(error.message);
    process.exitCode = 1;
    if (process.platform !== 'win32') {
      for (const row of owned.values()) { try { signal(row, 'SIGKILL'); } catch {} }
    }
  } finally {
    clearTimeout(cleanupDeadline);
    child.stdin.destroy();
    child.stdout.destroy();
    child.stderr.destroy();
    child.unref();
  }
  if (finalReport) {
    finalReport.passed = finalReport.passed && process.exitCode !== 1;
    console.log(JSON.stringify(finalReport));
    if (mode === 'new') writeFileSync(statePath, JSON.stringify({ sessionId, token: finalState.token, report: finalReport }, null, 2) + '\n');
  }
}
