import { spawn, execFileSync } from 'node:child_process';
import { readFileSync, writeFileSync, chmodSync, mkdtempSync, existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createHash } from 'node:crypto';
import { setTimeout as delay } from 'node:timers/promises';
import assert from 'node:assert/strict';

// Usage: node test-restart-runtime-slow.mjs NEW_HELPER [PREVIOUS_HELPER]
// Slow only full-table ps calls, using a preload confined to fixture helpers.
const [helper, previousHelper] = process.argv.slice(2);
assert(helper, 'Expected a helper path');
const tmux = '/opt/homebrew/bin/tmux';
const quote = value => "'" + value.replaceAll("'", "'\\''") + "'";
function identity(pid) {
  let text;
  try {
    text = execFileSync('/bin/ps', ['-p', String(pid), '-o', 'pid=,ppid=,stat=,lstart=,comm='], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] }).trim();
  } catch (error) {
    if (error.status === 1 && !String(error.stdout ?? '').trim() && !String(error.stderr ?? '').trim()) return null;
    throw error;
  }
  const match = text.match(/^(\d+)\s+(\d+)\s+(\S+)\s+(\w+\s+\w+\s+\d+\s+\d+:\d+:\d+\s+\d+)\s+(.+)$/);
  assert(match, 'Unrecognized fixture process metadata');
  return { pid: +match[1], ppid: +match[2], state: match[3], started: match[4].replace(/\s+/g, ' '), comm: match[5] };
}
const same = (a, b) => a && b && a.pid === b.pid && a.started === b.started && a.comm === b.comm && !a.state.includes('Z');
function signal(saved, name) {
  if (same(identity(saved.pid), saved)) {
    try { process.kill(saved.pid, name); } catch (error) { if (error.code !== 'ESRCH') throw error; }
  }
}
const cases = [
  ...(previousHelper ? [{ name: 'previous-slow', script: previousHelper, slow: 'slow', success: false }] : []),
  { name: 'targeted-slow', script: helper, slow: 'slow', success: true },
  { name: 'timeout-resumes-all', script: helper, slow: 'timeout', success: false },
];
for (const [index, scenario] of cases.entries()) {
  const dir = mkdtempSync(join(tmpdir(), 'misa-restart-slow-'));
  const socket = 'misa-restart-slow-' + process.pid + '-' + index;
  const records = join(dir, 'processes.jsonl');
  const fixture = join(dir, 'fixture.mjs');
  writeFileSync(fixture, `import {spawn} from 'node:child_process';
import {appendFileSync} from 'node:fs';
const record = pid => appendFileSync(process.argv[2], JSON.stringify({pid})+'\\n');
record(process.pid);
for(let i=0;i<12;i++){const child=spawn(process.execPath,['-e','setInterval(()=>{},1000)'],{detached:true,stdio:'ignore'});record(child.pid);child.unref();}
setInterval(()=>{},1000);
`);
  const runner = join(dir, 'runner.sh');
  writeFileSync(runner, '#!/bin/sh\nexec ' + [process.execPath, fixture, records].map(quote).join(' ') + '\n');
  chmodSync(runner, 0o700);
  const exitFile = join(dir, 'exit.json');
  const preload = join(dir, 'slow-ps.mjs');
  writeFileSync(preload, `import cp from 'node:child_process';
import {syncBuiltinESMExports} from 'node:module';
import {writeFileSync} from 'node:fs';
const originalExec=cp.execFileSync;
const originalKill=process.kill;
let stopping=false;
process.kill=function(pid,signal){if(signal==='SIGSTOP')stopping=true;return originalKill.call(process,pid,signal);};
cp.execFileSync=function(file,args,options){
 if(file==='/bin/ps'&&args[0]==='-axo'){
  const ms=${JSON.stringify(scenario.slow)}==='slow'?900:(stopping?6000:0);
  if(ms)Atomics.wait(new Int32Array(new SharedArrayBuffer(4)),0,0,ms);
 }
 return originalExec.call(cp,file,args,options);
};
syncBuiltinESMExports();
process.on('exit',code=>writeFileSync(${JSON.stringify(exitFile)},JSON.stringify({code})));
`);
  const list = () => existsSync(records) ? readFileSync(records, 'utf8').trim().split('\n').filter(Boolean).map(JSON.parse) : [];
  const unrelated = spawn(process.execPath, ['-e', 'setInterval(()=>{},1000)'], { stdio: 'ignore' });
  let unrelatedIdentity;
  let controller;
  const cleanupIdentities = new Map();
  try {
    unrelatedIdentity = identity(unrelated.pid);
    execFileSync(tmux, ['-L', socket, 'new-session', '-d', '-s', 'fixture', runner]);
    const ready = Date.now() + 5000;
    while (Date.now() < ready && list().length < 13) await delay(50);
    assert.equal(list().length, 13);
    const old = list().map(row => identity(row.pid));
    assert(old.every(Boolean));
    for (const row of old) cleanupIdentities.set(row.pid, row);
    const root = old[0];
    controller = identity(root.ppid);
    const mapping = join(dir, 'mapping.json');
    writeFileSync(mapping, JSON.stringify({ 'test:thread': 'test-session' }));
    const resultPath = join(dir, 'result.json');
    const spec = { armed: true, model: 'test-only', delayMs: 100, cwd: dir, mappingPath: mapping, mappingBackup: join(dir, 'backup.json'), threadKey: 'test:thread', sessionId: 'test-session', resultPath, artifacts: [{ path: runner, sha256: createHash('sha256').update(readFileSync(runner)).digest('hex') }], services: [{ socket, session: 'fixture', runner, root }] };
    const specPath = join(dir, 'spec.json');
    writeFileSync(specPath, JSON.stringify(spec));
    const log = join(dir, 'helper.log');
    const command = [process.execPath, '--import', preload, scenario.script, specPath].map(quote).join(' ') + ' > ' + quote(log) + ' 2>&1';
    const started = Date.now();
    execFileSync(tmux, ['-L', socket, 'new-session', '-d', '-s', 'helper', command]);
    const deadline = Date.now() + 65000;
    while (Date.now() < deadline && !existsSync(exitFile)) await delay(100);
    assert(existsSync(exitFile), 'Helper exceeded fixture deadline: ' + dir);
    const output = readFileSync(log, 'utf8');
    const { code } = JSON.parse(readFileSync(exitFile, 'utf8'));
    assert.equal(code === 0, scenario.success, output);
    assert(same(identity(unrelated.pid), unrelatedIdentity), 'Unrelated process affected');
    assert(same(identity(controller.pid), controller) && !identity(controller.pid).state.includes('T'), 'Controller not resumed');
    assert.equal(JSON.parse(readFileSync(mapping, 'utf8'))['test:thread'], 'test-session');
    if (scenario.success) {
      assert(old.every(row => !same(identity(row.pid), row)), 'Old process survived');
      const result = JSON.parse(readFileSync(resultPath, 'utf8'));
      assert.notEqual(result.services[0].pid, root.pid);
      assert(identity(result.services[0].pid));
    } else {
      assert(output.includes('Runtime tree did not quiesce'), output);
      assert(!existsSync(resultPath), 'Replacement must not start on failure');
      assert(old.every(row => { const now = identity(row.pid); return same(now, row) && !now.state.includes('T'); }), 'Failure did not resume every old process');
    }
    console.log(JSON.stringify({ case: scenario.name, passed: true, elapsedMs: Date.now() - started, oldProcessCount: old.length, oldStopped: scenario.success, allResumedOnFailure: !scenario.success, unrelatedPreserved: true, controllerResumed: true, mappingPreserved: true, fixtureDirectory: dir }));
  } finally {
    if (controller) signal(controller, 'SIGCONT');
    for (const row of list()) if (!cleanupIdentities.has(row.pid)) { const current = identity(row.pid); if (current) cleanupIdentities.set(row.pid, current); }
    for (const row of cleanupIdentities.values()) { signal(row, 'SIGCONT'); signal(row, 'SIGKILL'); }
    if (unrelatedIdentity) signal(unrelatedIdentity, 'SIGKILL');
    try { execFileSync(tmux, ['-L', socket, 'kill-server'], { stdio: 'ignore', timeout: 3000 }); } catch {}
  }
}
