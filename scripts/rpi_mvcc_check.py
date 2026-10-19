#!/usr/bin/env python3
"""Reproduce MVCC benchmark with native --rpi and direct DeepSeek.

prepare is offline; run incurs bounded provider usage; verify uses the existing
independent container verifier. Original task and verifier trees stay untouched.
"""
import argparse
import datetime
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import time

ROOT=Path(__file__).resolve().parents[1]
CACHE=Path.home()/'.cache/hx-tb4'
TASK=CACHE/'repo/tasks/mvcc-lsm-compaction'
VERIFIER_IMAGE='tb4-mvcc-verifier:latest'


def save(path,data):path.write_text(json.dumps(data,indent=2)+'\n')


def prepare(run, args=None):
    if not (TASK/'environment/app').is_dir():raise SystemExit('Missing benchmark checkout: '+str(TASK))
    run.mkdir(parents=True,exist_ok=False)
    shutil.copytree(TASK/'environment/app',run/'ws')
    ws = run/'ws'
    subprocess.run(['git','init','-q'],cwd=ws,check=True)
    subprocess.run(['git','config','user.email','bench@test.local'],cwd=ws,check=True)
    subprocess.run(['git','config','user.name','benchmark'],cwd=ws,check=True)
    (ws/'.gitignore').write_text('build/\n*.o\n*.dSYM/\n')
    subprocess.run(['git','add','.'],cwd=ws,check=True)
    subprocess.run(['git','commit','-q','-m','Initial benchmark baseline'],cwd=ws,check=True)
    prompt='\n'.join(line for line in (TASK/'instruction.md').read_text().splitlines() if 'harbor-canary' not in line)
    for old,new in [('-C /app','-C .'),('`/app/`','the current directory'),('`/app/','`'),('`/app`','the current directory'),(' /app/',' ')]:
        prompt=prompt.replace(old,new)
    (run/'prompt.md').write_text(prompt+'\n\nThe task directory /app is the current directory; use relative paths.\n'
        'Use the native research/design/implementation/verification workflow. Preserve public C++ APIs. '
        'Validate both published visibility and every still-publishable future version, including tombstones. '
        'Add meaningful regressions that compile and fail against the original implementation, '
        'then pass after repair. Avoid keeping all obsolete history.\n')
    model = getattr(args, 'model', 'deepseek-flash') if args else 'deepseek-flash'
    api = getattr(args, 'api', None) or ('responses' if 'deepseek' in model else 'completions')
    base_url = getattr(args, 'base_url', None) or ('https://api.deepseek.com' if 'deepseek' in model else 'https://openrouter.ai/api/v1')
    save(run/'case.json',{'created_at':datetime.datetime.now(datetime.timezone.utc).isoformat(),
        'baseline':str(TASK/'environment/app'),'task_commit':subprocess.check_output(
            ['git','rev-parse','HEAD'],cwd=TASK,text=True).strip(),
        'baseline_source':'original task, not a previous generated solution',
        'verifier_image':VERIFIER_IMAGE,'verifier_source':str(TASK/'tests'),
        'model':model,'api':api,'base_url':base_url})
    print('Prepared: '+str(run),flush=True)


def verify(run):
    grade=run/'grade'
    if grade.exists():raise SystemExit('Grade directory already exists; preserve it or choose a new run')
    shutil.copytree(run/'ws',grade/'app',ignore=shutil.ignore_patterns('.harness','.agents','.validation','.venv','__pycache__','build','.git'))
    (grade/'logs').mkdir()
    argv=['container','run','--rm','-v',str(grade/'app')+':/app','-v',str(grade/'logs')+':/logs',
          VERIFIER_IMAGE,'bash','/tests/test.sh']
    started=time.monotonic()
    with (run/'grade.log').open('w') as out:
        try:
            result=subprocess.run(argv,stdout=out,stderr=subprocess.STDOUT,timeout=900)
            code=result.returncode
        except subprocess.TimeoutExpired:code=124
    reward=grade/'logs/verifier/reward.txt'
    value=reward.read_text().strip() if reward.exists() else None
    report={'exit_code':code,'reward':value,'passed':code==0 and value=='1',
            'elapsed_secs':time.monotonic()-started,'log':str(run/'grade.log')}
    save(run/'verification.json',report)
    print(json.dumps(report),flush=True)
    if not report['passed']:raise SystemExit(1)


def run_agent(run,args):
    model = args.model
    api = args.api or ('responses' if 'deepseek' in model else 'completions')
    base_url = args.base_url or ('https://api.deepseek.com' if 'deepseek' in model else 'https://openrouter.ai/api/v1')
    key_env = args.key_env or ('DEEPSEEK_API_KEY' if 'deepseek' in model else 'OPENROUTER_API_KEY')
    if not os.environ.get(key_env):raise SystemExit(f'{key_env} is missing')
    context_window = str(args.context_window or (1048576 if 'deepseek' in model else 1000000))
    argv=[str(Path(args.hx).resolve()),'run','--rpi','--workspace',str(run/'ws'),
        '--prompt-file',str(run/'prompt.md'),'--model',model,'--api',api,
        '--base-url',base_url,'--key-env',key_env,'--reasoning',args.reasoning,
        '--max-cost',str(args.max_cost),'--max-wall',str(args.max_wall),
        '--max-turns','200','--max-output','16384','--context-window',context_window,'--trace',str(run/'trace.md')]
    if args.max_tokens is not None:
        argv.extend(['--max-tokens',str(args.max_tokens)])
    save(run/'launch.json',{'argv':argv,'harness_commit':subprocess.check_output(
        ['git','rev-parse','HEAD'],cwd=ROOT,text=True).strip(),
        'started_at':datetime.datetime.now(datetime.timezone.utc).isoformat()})
    started=time.monotonic()
    with (run/'result.json').open('w') as out,(run/'stderr.log').open('w') as err:
        proc=subprocess.Popen(argv,cwd=ROOT,env={**os.environ,'HX_TRUST':'1'},stdout=out,stderr=err,start_new_session=True)
        try:code=proc.wait(timeout=args.max_wall+90)
        except subprocess.TimeoutExpired:
            os.killpg(proc.pid,signal.SIGTERM)
            try:proc.wait(timeout=10)
            except subprocess.TimeoutExpired:os.killpg(proc.pid,signal.SIGKILL);proc.wait()
            code=124
    save(run/'process.json',{'exit_code':code,'elapsed_secs':time.monotonic()-started})
    if code:raise SystemExit('Agent failed; inspect '+str(run/'stderr.log'))
    report=json.loads((run/'result.json').read_text())
    print(json.dumps({k:report.get(k) for k in ['finished','status','stopped','cost_usd','wall_secs','llm_calls']}),flush=True)
    verify(run)
    if not report.get('finished'):raise SystemExit('Verifier passed but workflow did not finish')


def main():
    global TASK, VERIFIER_IMAGE
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('action',choices=['prepare','run','verify'])
    p.add_argument('--run-dir',type=Path)
    p.add_argument('--task-dir',type=Path,default=TASK)
    p.add_argument('--verifier-image',default=VERIFIER_IMAGE)
    p.add_argument('--hx',default=str(ROOT/'target/release/hx'))
    p.add_argument('--model',default='deepseek-flash')
    p.add_argument('--api',default=None)
    p.add_argument('--base-url',default=None)
    p.add_argument('--key-env',default=None)
    p.add_argument('--context-window',type=int,default=None)
    p.add_argument('--max-cost',type=float,default=2.0)
    p.add_argument('--max-tokens',type=int,default=None)
    p.add_argument('--max-wall',type=int,default=900)
    p.add_argument('--reasoning',default='medium')
    args=p.parse_args()
    TASK=args.task_dir.resolve()
    VERIFIER_IMAGE=args.verifier_image
    if args.action!='prepare' and args.run_dir is None:p.error('--run-dir required for run/verify')
    run=(args.run_dir or CACHE/'runs'/('mvcc-native-rpi-'+datetime.datetime.now().strftime('%Y%m%d-%H%M%S'))).resolve()
    if args.action=='prepare':prepare(run,args)
    elif args.action=='run':run_agent(run,args)
    else:verify(run)


if __name__=='__main__':main()
