import assert from 'node:assert/strict';
import { spawn, execFileSync } from 'node:child_process';
import { mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const probe = process.argv[2] ?? fileURLToPath(new URL('./verify-acp-upgrade.mjs', import.meta.url));
const dir = mkdtempSync(join(tmpdir(), 'misa-acp-probe-proof-'));
const fixture = join(dir, 'fake-acp.mjs');
writeFileSync(fixture, `
import { createInterface } from 'node:readline';
import { spawn } from 'node:child_process';
import { appendFileSync } from 'node:fs';
const mode = process.argv[2];
const records = process.argv[3];
const send = value => process.stdout.write(JSON.stringify({jsonrpc:'2.0',...value})+'\\n');
const result = (id, result) => send({id,result});
const update = update => send({method:'session/update',params:{sessionId:'test-session',update}});
const text = text => update({sessionUpdate:'agent_message_chunk',content:{type:'text',text}});
let primary;
let token;
if (['ignore-term','early-exit'].includes(mode)) {
  process.on('SIGTERM',()=>{});
  const child = spawn(process.execPath,['-e','process.on("SIGTERM",()=>{});setInterval(()=>{},1000)'],{detached:true,stdio:'ignore'});
  appendFileSync(records,JSON.stringify({pid:child.pid})+'\\n');
  child.unref();
}
createInterface({input:process.stdin}).on('line',line=>{
  const request=JSON.parse(line);
  if(request.method==='initialize')return result(request.id,{protocolVersion:1,agentCapabilities:{}});
  if(request.method==='session/new')return result(request.id,{sessionId:'test-session'});
  if(request.method==='session/set_config_option')return result(request.id,{configOptions:[{id:'model',currentValue:'test-model'}]});
  if(request.method!=='session/prompt')return;
  if(!primary){
    primary=request.id;token=request.params.prompt[0].text.match(/MISA_MEMORY_[a-f0-9]+/)[0];
    return setTimeout(()=>update({sessionUpdate:'tool_call',toolCallId:'test-tool',status:'in_progress'}),650);
  }
  update({sessionUpdate:'tool_call_update',toolCallId:'test-tool',status:'completed'});
  if(mode==='queued'){
    text('MISA_TOOL_OK '+token);result(primary,{stopReason:'end_turn'});
    text('MISA_STEER_OK');result(request.id,{stopReason:'end_turn'});
  }else{
    text('MISA_TOOL_OK '+token+' MISA_STEER_OK');
    result(primary,{stopReason:'end_turn'});result(request.id,{stopReason:'end_turn'});
  }
  if(mode==='early-exit')process.stdout.write('',()=>process.exit(0));
});
`);
const alive = pid => {
  try {
    if (process.platform !== 'win32') return !execFileSync('ps', ['-p', String(pid), '-o', 'stat='], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'] }).trim().includes('Z');
    process.kill(pid, 0); return true;
  } catch { return false; }
};
const unrelated = spawn(process.execPath, ['-e', 'setInterval(()=>{},1000)'], { stdio: 'ignore' });
const recorded = [];
try {
  for (const mode of process.platform === 'win32' ? ['positive', 'queued', 'ignore-term'] : ['positive', 'queued', 'ignore-term', 'early-exit']) {
    const records = join(dir, mode + '-children.jsonl');
    const spec = join(dir, mode + '-spec.json');
    writeFileSync(records, '');
    writeFileSync(spec, JSON.stringify({ command: process.execPath, args: [fixture, mode, records], working_dir: dir, model: 'test-model' }));
    const started = Date.now();
    const child = spawn(process.execPath, [probe, spec, join(dir, mode + '-state.json'), 'new'], { stdio: ['ignore', 'pipe', 'pipe'] });
    let output = '';
    child.stdout.on('data', data => { output += data; });
    child.stderr.on('data', data => { output += data; });
    const code = await new Promise((resolve, reject) => {
      const timeout = setTimeout(() => { child.kill('SIGKILL'); reject(new Error('Probe did not exit within 12 seconds: ' + mode)); }, 12000);
      child.once('error', reject);
      child.once('exit', code => { clearTimeout(timeout); resolve(code); });
    });
    const rows = readFileSync(records, 'utf8').split('\n').filter(Boolean).map(JSON.parse);
    recorded.push(...rows);
    assert.equal(code, mode === 'queued' ? 1 : 0, mode + ': ' + output);
    const result = output.split('\n').filter(line => line.startsWith('{')).map(JSON.parse).find(row => row.event === 'result');
    assert.equal(result.passed, mode !== 'queued', mode);
    assert(rows.every(row => !alive(row.pid)), 'Detached child survived: ' + mode);
    assert(alive(unrelated.pid), 'Unrelated process was affected');
    console.log(JSON.stringify({ mode, exitCode: code, passed: result.passed, elapsedMs: Date.now() - started, detachedChildrenStopped: rows.length, unrelatedPreserved: true }));
  }
} finally {
  for (const row of recorded) { try { process.kill(row.pid, 'SIGKILL'); } catch {} }
  try { unrelated.kill('SIGKILL'); } catch {}
}
