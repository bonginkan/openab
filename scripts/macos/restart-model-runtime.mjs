import { execFileSync } from 'node:child_process';
import { readFileSync, writeFileSync, copyFileSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { setTimeout as delay } from 'node:timers/promises';

const spec = JSON.parse(readFileSync(process.argv[2], 'utf8'));
const dryRun = process.argv.includes('--preflight');
if (!dryRun && spec.armed !== true) throw new Error('Restart specification is not armed');
const tmux = '/opt/homebrew/bin/tmux';
const log = event => console.log(JSON.stringify({ time: new Date().toISOString(), ...event }));
const hash = path => createHash('sha256').update(readFileSync(path)).digest('hex');

function processes(pid) {
  const fields = 'pid=,ppid=,pgid=,stat=,lstart=,comm=';
  const args = pid === undefined ? ['-axo', fields] : ['-p', String(pid), '-o', fields];
  let output;
  try {
    output = execFileSync('/bin/ps', args, { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] });
  } catch (error) {
    // ps exits 1 with empty output when the selected process has already exited.
    if (pid !== undefined && error.status === 1 && !String(error.stdout ?? '').trim()
        && !String(error.stderr ?? '').trim()) return [];
    throw error;
  }
  return output.trim().split('\n').filter(Boolean).map(line => {
      const match = line.trim().match(/^(\d+)\s+(\d+)\s+(\d+)\s+(\S+)\s+(\w+\s+\w+\s+\d+\s+\d+:\d+:\d+\s+\d+)\s+(.+)$/);
      if (!match) throw new Error('Unrecognized process metadata');
      return { pid: Number(match[1]), ppid: Number(match[2]), pgid: Number(match[3]), state: match[4], started: match[5].replace(/\s+/g, ' '), comm: match[6] };
    });
}

function sameProcess(saved) {
  return processes(saved.pid).some(row => matches(row, saved) && !row.state.includes('Z'));
}

function matches(row, saved) {
  return row.pid === saved.pid && row.started === saved.started && row.comm === saved.comm;
}

function preflight() {
  for (const artifact of spec.artifacts) {
    if (hash(artifact.path) !== artifact.sha256) throw new Error('Artifact changed: ' + artifact.path);
  }
  const rows = processes();
  const ids = new Set();
  for (const service of spec.services) {
    const root = rows.find(row => row.pid === service.root.pid);
    if (!root || root.started !== service.root.started || root.comm !== service.root.comm) throw new Error('Service identity changed');
    const panePid = execFileSync(tmux, ['-L', service.socket, 'display-message', '-p', '-t', service.session + ':0.0', '#{pane_pid}'], { encoding: 'utf8' }).trim();
    if (panePid !== String(root.pid)) throw new Error('Service pane changed');
    ids.add(root.pid);
  }
  for (let n = 0; n < rows.length; n++) {
    const count = ids.size;
    for (const row of rows) if (ids.has(row.ppid)) ids.add(row.pid);
    if (ids.size === count) break;
  }
  if (!dryRun && (ids.has(process.pid) || ids.has(process.ppid))) throw new Error('Restart helper must run outside the affected trees');
  const mapping = JSON.parse(readFileSync(spec.mappingPath, 'utf8'));
  if (mapping[spec.threadKey] !== spec.sessionId) throw new Error('Current thread mapping changed');
  return rows.filter(row => ids.has(row.pid));
}

function signalIfSame(saved, signal) {
  if (!sameProcess(saved)) return;
  try { process.kill(saved.pid, signal); } catch (error) { if (error.code !== 'ESRCH') throw error; }
}

async function quiesce(initial) {
  const known = new Map(initial.map(row => [row.pid, row]));
  const stoppedByUs = new Map();
  const resume = () => { for (const row of stoppedByUs.values()) signalIfSame(row, 'SIGCONT'); };
  const freeze = row => {
    if (!row.state.includes('T')) {
      stoppedByUs.set(row.pid, row);
      signalIfSame(row, 'SIGSTOP');
    }
  };
  try {
    // tmux resumes stopped pane leaders. Pause only the verified owning server
    // while freezing its pane; helper and unrelated pane processes keep running.
    const controllers = spec.services.map(service => {
      const serverPid = Number(execFileSync(tmux, ['-L', service.socket, 'display-message', '-p', '#{pid}'], { encoding: 'utf8' }).trim());
      const row = processes().find(item => item.pid === serverPid);
      if (!row || row.pid !== service.root.ppid) throw new Error('Owning tmux server changed');
      return row;
    });
    for (const row of controllers) freeze(row);
    // Stop intake first, then discover descendants again after their parents stop.
    for (const service of spec.services) freeze(initial.find(row => matches(row, service.root)));
    let stable = 0;
    let observations = 0;
    const started = Date.now();
    const deadline = started + 10000;
    while (Date.now() < deadline) {
      const rows = processes();
      observations++;
      const alive = rows.filter(row => known.has(row.pid) && matches(row, known.get(row.pid)) && !row.state.includes('Z'));
      const owners = new Set(alive.map(row => row.pid));
      // A live owned group leader anchors membership, including reparented children.
      const groups = new Set(alive.filter(row => row.pid === row.pgid).map(row => row.pgid));
      let added = false;
      for (const row of rows) {
        if (!owners.has(row.pid) && (owners.has(row.ppid) || groups.has(row.pgid))) {
          if (row.pid === process.pid || row.pid === process.ppid) throw new Error('Helper entered affected tree');
          known.set(row.pid, row);
          owners.add(row.pid);
          added = true;
        }
        if (owners.has(row.pid) && !row.state.includes('Z')) freeze(row);
      }
      const allStopped = rows.filter(row => owners.has(row.pid)).every(row => row.state.includes('T') || row.state.includes('Z'));
      stable = !added && allStopped ? stable + 1 : 0;
      if (stable >= 2) return { owned: [...known.values()], resume };
      await delay(25);
    }
    const remaining = processes().filter(row => known.has(row.pid) && matches(row, known.get(row.pid)));
    throw new Error('Runtime tree did not quiesce: ' + JSON.stringify({ elapsedMs: Date.now() - started, observations, remaining: remaining.map(({ pid, state }) => ({ pid, state })) }));
  } catch (error) {
    resume();
    throw error;
  }
}

const initial = preflight();
log({ event: 'preflight_ok', dryRun, serviceCount: spec.services.length, processCount: initial.length });
if (!dryRun) {
  await delay(spec.delayMs);
  const { owned, resume } = await quiesce(preflight());
  // Keep the tree stopped through termination; resuming permits another fork race.
  // Session IDs are persisted when connections are created, before this restart.
  try {
    const frozenMapping = JSON.parse(readFileSync(spec.mappingPath, 'utf8'));
    if (frozenMapping[spec.threadKey] !== spec.sessionId) throw new Error('Thread mapping changed before termination');
    copyFileSync(spec.mappingPath, spec.mappingBackup);
  } catch (error) {
    resume();
    throw error;
  }
  try {
    for (const row of owned) signalIfSame(row, 'SIGKILL');
  } finally {
    resume();
  }
  const deadline = Date.now() + 5000;
  while (Date.now() < deadline && owned.some(sameProcess)) await delay(100);
  if (owned.some(sameProcess)) throw new Error('Old runtime processes remain');
  const after = JSON.parse(readFileSync(spec.mappingPath, 'utf8'));
  if (after[spec.threadKey] !== spec.sessionId) throw new Error('Thread mapping was not preserved');
  for (const service of spec.services) {
    let exists = false;
    try { execFileSync(tmux, ['-L', service.socket, 'has-session', '-t', service.session], { stdio: 'ignore' }); exists = true; } catch {}
    if (exists) {
      execFileSync(tmux, ['-L', service.socket, 'respawn-pane', '-t', service.session + ':0.0', '-c', spec.cwd, service.runner]);
    } else {
      execFileSync(tmux, ['-L', service.socket, 'new-session', '-d', '-s', service.session, '-c', spec.cwd, service.runner]);
    }
  }
  await delay(5000);
  const result = spec.services.map(service => {
    const pid = Number(execFileSync(tmux, ['-L', service.socket, 'display-message', '-p', '-t', service.session + ':0.0', '#{pane_pid}'], { encoding: 'utf8' }).trim());
    const row = processes().find(item => item.pid === pid);
    if (!row || row.pid === service.root.pid || row.comm !== service.root.comm) throw new Error('New service did not start');
    return { socket: service.socket, session: service.session, ...row };
  });
  writeFileSync(spec.resultPath, JSON.stringify({ services: result, threadKey: spec.threadKey, sessionId: spec.sessionId, model: spec.model, status: 'processes_started_model_response_pending' }, null, 2) + '\n', { mode: 0o600 });
  log({ event: 'services_restarted', services: result, modelResponseVerified: false });
}
