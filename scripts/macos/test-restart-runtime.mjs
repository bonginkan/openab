import { spawn, execFileSync } from 'node:child_process';
import { readFileSync, writeFileSync, chmodSync, mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createHash } from 'node:crypto';
import { setTimeout as delay } from 'node:timers/promises';
import assert from 'node:assert/strict';

const helper = process.argv[2];
const dir = mkdtempSync(join(tmpdir(), 'misa-restart-proof-'));
const socket = 'misa-restart-proof-' + process.pid;
const tmux = '/opt/homebrew/bin/tmux';
const records = join(dir, 'processes.jsonl');
const fixture = join(dir, 'fixture.mjs');
writeFileSync(fixture, `import {spawn} from 'node:child_process';
import {appendFileSync} from 'node:fs';
const record = item => appendFileSync(process.argv[2], JSON.stringify(item)+'\\n');
record({kind:'root',pid:process.pid});
function child() { const c=spawn(process.execPath,['-e','setInterval(()=>{},1000)'],{detached:true,stdio:'ignore'}); record({kind:'child',pid:c.pid,parent:process.pid}); c.unref(); }
child(); let count=0; const interval=setInterval(()=>{if(count++<4)child();else clearInterval(interval)},100);
process.on('SIGTERM',()=>{setTimeout(child,50)});
setInterval(()=>{},1000);
`);
const runner = join(dir, 'runner.sh');
writeFileSync(runner, `#!/bin/sh\nexec '${process.execPath}' '${fixture}' '${records}'\n`);
chmodSync(runner, 0o700);
const list = () => { try { return readFileSync(records,'utf8').trim().split('\n').filter(Boolean).map(JSON.parse); } catch { return []; } };
const alive = pid => { try { return !execFileSync('/bin/ps',['-p',String(pid),'-o','stat='],{encoding:'utf8',stdio:['ignore','pipe','ignore']}).trim().startsWith('Z'); } catch { return false; } };
const kill = pid => { try { process.kill(pid,'SIGKILL'); } catch(error) { if(error.code!=='ESRCH')throw error; } };
const unrelated = spawn(process.execPath,['-e','setInterval(()=>{},1000)'],{stdio:'ignore'});
try {
  // Reproduce the old snapshot gap: the TERM handler creates a detached child.
  const old = spawn(process.execPath,[fixture,records],{stdio:'ignore'});
  await delay(650);
  const oldSnapshot = list().filter(row => row.pid===old.pid || row.parent===old.pid);
  old.kill('SIGTERM');
  await delay(200);
  const late = list().filter(row => row.parent===old.pid && !oldSnapshot.some(saved=>saved.pid===row.pid));
  assert(late.length>0 && late.every(row=>alive(row.pid)));
  for(const row of oldSnapshot)kill(row.pid);
  assert(late.some(row=>alive(row.pid)));
  for(const row of late)kill(row.pid);

  execFileSync(tmux,['-L',socket,'new-session','-d','-s','fixture',runner]);
  await delay(200);
  const pid=Number(execFileSync(tmux,['-L',socket,'display-message','-p','-t','fixture:0.0','#{pane_pid}'],{encoding:'utf8'}).trim());
  const readyDeadline=Date.now()+5000;
  while(Date.now()<readyDeadline && !list().some(row=>row.kind==='root' && row.pid===pid))await delay(50);
  assert(list().some(row=>row.kind==='root' && row.pid===pid));
  const line=execFileSync('/bin/ps',['-p',String(pid),'-o','pid=,ppid=,lstart=,comm='],{encoding:'utf8'}).trim();
  const match=line.match(/^(\d+)\s+(\d+)\s+(\w+\s+\w+\s+\d+\s+\d+:\d+:\d+\s+\d+)\s+(.+)$/);
  const root={pid:+match[1],ppid:+match[2],started:match[3].replace(/\s+/g,' '),comm:match[4]};
  const mapping=join(dir,'mapping.json');
  writeFileSync(mapping,JSON.stringify({'test:thread':'test-session'}));
  const spec={armed:true,model:'test-only',delayMs:400,cwd:dir,mappingPath:mapping,mappingBackup:join(dir,'backup.json'),threadKey:'test:thread',sessionId:'test-session',resultPath:join(dir,'result.json'),artifacts:[{path:runner,sha256:createHash('sha256').update(readFileSync(runner)).digest('hex')}],services:[{socket,session:'fixture',runner,root}]};
  const specPath=join(dir,'spec.json');writeFileSync(specPath,JSON.stringify(spec));
  const output=execFileSync(process.execPath,[helper,specPath],{encoding:'utf8',timeout:25000});
  const oldRows=list().filter(row=>row.pid===pid || row.parent===pid);
  assert(oldRows.length>=3);
  assert(oldRows.every(row=>!alive(row.pid)));
  assert(alive(unrelated.pid));
  const result=JSON.parse(readFileSync(spec.resultPath,'utf8'));
  assert(result.services[0].pid!==pid && alive(result.services[0].pid));
  assert.equal(JSON.parse(readFileSync(mapping,'utf8'))['test:thread'],'test-session');
  console.log(JSON.stringify({status:'pass',oldSnapshotMissedChildren:late.length,stoppedOldProcesses:oldRows.length,unrelatedPreserved:true,newRuntimeStarted:true,mappingPreserved:true,fixtureDirectory:dir,helperOutput:output.trim().split('\n').map(JSON.parse)},null,2));
} finally {
  for(const row of list())kill(row.pid);
  kill(unrelated.pid);
  try { execFileSync(tmux,['-L',socket,'kill-server'],{stdio:'ignore'}); } catch {}
}
