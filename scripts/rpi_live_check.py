#!/usr/bin/env python3
"""Opt-in paid RPI evaluation: prepare, run, or independently verify a Bun app.

No provider calls occur in prepare/verify. Run requires DEEPSEEK_API_KEY and
explicit --run-dir. All provider output and verifier evidence remain on disk.
"""
import argparse
import datetime
import http.cookiejar
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import sqlite3
import subprocess
import tempfile
import time
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
BRIEF = """Build Finch, a working Twitter-like local app. Complete research, design,
implementation, and verification; make reasonable reversible choices autonomously.
Use Bun.serve, bun:sqlite, and vanilla HTML/CSS/JS, no external npm packages.
Deliver server.ts, browser assets, README startup/API notes, and meaningful bun test
integration tests using an ephemeral listening port and temporary SQLite database.
Start with `bun server.ts`; bind 127.0.0.1, respect PORT and DB_PATH environment
variables. PORT defaults to 3000. DB_PATH defaults to ./finch.sqlite. Never reset a
populated database on startup. Use Bun.password (or equivalent secure password
hashing), opaque random server-side session tokens in HttpOnly SameSite cookies,
prepared SQL, robust error statuses, and escape untrusted content in the browser.
Provide a polished responsive UI: signup/login/logout, tweet composer with 280
character count, newest-first feed with load-more pagination, like/unlike, and
profiles. UI needs real API integration and useful pending/error/empty states.

The following API contract is required for an independent external verifier:
* JSON requests/responses; errors return {"error":"message"}. No credentials or
  hashes in API responses. All tweet ids and user ids may be numbers or strings.
* POST /api/signup {username,password,displayName} -> 201 {user}, session cookie.
  username is 3-20 ASCII alphanumeric/underscore chars, password at least 8 chars.
  duplicate username -> 409, malformed/invalid JSON input -> 400.
* POST /api/login {username,password} -> 200 {user}, session cookie; wrong -> 401.
* POST /api/logout -> 200 or 204 and revoke session (not just remove cookie).
* GET /api/me -> 200 {user}, or 401 when unauthenticated.
* POST /api/tweets {text} -> 201 {tweet}. Auth required; text must be a string,
  nonblank, at most 280 Unicode code points; invalid -> 400. No implicit coercion.
* GET /api/tweets?limit=N&before=ID -> 200 {tweets:[...],nextCursor:ID|null}.
  Public, newest first, limit clamped to 1-50 with sensible default; before is an
  exclusive id cursor; pages must have no duplicates. Each tweet includes id,
  text, username, displayName, likes (integer count), liked (viewer boolean).
* PUT /api/tweets/:id/like -> 200 JSON; auth required, idempotently add own like.
* DELETE /api/tweets/:id/like -> 200 JSON; auth required, idempotently remove it.
  Missing tweet -> 404. Feed reflects resulting count and viewer liked boolean.
* GET /api/users/:username -> 200 {user:{username,displayName},tweets:[...]} or 404.
* Unknown /api route -> 404 JSON; unsupported method must not mutate state.
* GET / serves HTML with linked JS/CSS implementing these features.

Test security and persistence, including unauthorized mutations, duplicate signup,
bad login, Unicode tweet length, SQL-looking content, safe rendering, double likes,
feed pagination, logout revocation and restart against the same database. Run your
own tests and fix failures. Keep scope local; no deployment or external services.
"""


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n")


def prepare(run_dir):
    run_dir.mkdir(parents=True, exist_ok=False)
    ws = run_dir / 'workspace'
    ws.mkdir()
    subprocess.run(['git', 'init', '-q'], cwd=ws, check=True)
    subprocess.run(['git', 'config', 'user.email', 'test@test.local'], cwd=ws, check=True)
    subprocess.run(['git', 'config', 'user.name', 'test'], cwd=ws, check=True)
    (run_dir / 'prompt.md').write_text(BRIEF)
    write_json(run_dir / 'case.json', {'created_at':datetime.datetime.now(datetime.timezone.utc).isoformat(),
        'model':'deepseek-flash','base_url':'https://api.deepseek.com',
        'docs':['https://api-docs.deepseek.com/api/create-response/'],
        'independent_verifier':str(Path(__file__).resolve())})
    print(run_dir)


def run_agent(run_dir, args):
    if not os.environ.get('DEEPSEEK_API_KEY'):
        raise SystemExit('DEEPSEEK_API_KEY is missing')
    argv = [str(Path(args.hx).resolve()), 'run', '--rpi', '--workspace', str(run_dir/'workspace'),
            '--prompt-file', str(run_dir/'prompt.md'), '--model','deepseek-flash',
            '--base-url','https://api.deepseek.com','--key-env','DEEPSEEK_API_KEY', '--reasoning',args.reasoning,
            '--max-wall',str(args.max_wall),'--max-turns','200','--max-output','16384',
            '--context-window','1048576','--trace',str(run_dir/'trace.md')]
    if args.max_tokens is not None:
        argv.extend(['--max-tokens', str(args.max_tokens)])
    write_json(run_dir/'launch.json',{'argv':argv,'commit':subprocess.check_output(
        ['git','rev-parse','HEAD'],cwd=ROOT,text=True).strip(),
        'started_at':datetime.datetime.now(datetime.timezone.utc).isoformat()})
    started=time.monotonic()
    with (run_dir/'result.json').open('w') as out, (run_dir/'stderr.log').open('w') as err:
        proc=subprocess.Popen(argv,cwd=ROOT,env={**os.environ,'HX_TRUST':'1'},stdout=out,stderr=err,start_new_session=True)
        try:
            code=proc.wait(timeout=args.max_wall+90)
        except subprocess.TimeoutExpired:
            os.killpg(proc.pid,signal.SIGTERM)
            try: proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                os.killpg(proc.pid,signal.SIGKILL);proc.wait()
            code=124
    write_json(run_dir/'process.json',{'exit_code':code,'elapsed_secs':time.monotonic()-started})
    if code:
        raise SystemExit('Agent failed; inspect '+str(run_dir/'stderr.log'))
    report=json.loads((run_dir/'result.json').read_text())
    print(json.dumps({k:report.get(k) for k in ['finished','status','stopped','wall_secs','llm_calls']}))
    verify(run_dir)
    if not report.get('finished'):
        raise SystemExit('External checks ran, but workflow did not finish; inspect result.json')


class Client:
    def __init__(self, base):
        self.base=base
        self.jar=http.cookiejar.CookieJar()
        self.opener=urllib.request.build_opener(urllib.request.HTTPCookieProcessor(self.jar))

    def request(self, method, path, data=None, raw=None, headers=None):
        body=raw if raw is not None else (json.dumps(data).encode() if data is not None else None)
        req=urllib.request.Request(self.base+path,method=method,data=body,
            headers={'Content-Type':'application/json',**(headers or {})})
        try: response=self.opener.open(req,timeout=5)
        except urllib.error.HTTPError as e: response=e
        with response:
            payload=response.read()
            try: payload=json.loads(payload)
            except (ValueError,UnicodeDecodeError): payload=payload.decode(errors='replace')
            return response.status,payload,response.headers


def verify(run_dir):
    """External black-box checks: never import or execute the app's own tests."""
    ws=run_dir/'workspace'
    checks=[]
    proc=None
    def check(name, condition):
        checks.append({'name':name,'passed':bool(condition)})
        if not condition: raise AssertionError(name)
    with tempfile.TemporaryDirectory(prefix='hx-rpi-verifier-') as tmp:
        db=Path(tmp)/'verify.sqlite'
        with socket.socket() as sock:
            sock.bind(('127.0.0.1',0));port=sock.getsockname()[1]
        base='http://127.0.0.1:'+str(port)
        log=(run_dir/'verifier-server.log').open('w')
        def start():
            nonlocal proc
            proc=subprocess.Popen(['bun','server.ts'],cwd=ws,env={**os.environ,'PORT':str(port),'DB_PATH':str(db)},
                stdout=log,stderr=subprocess.STDOUT,start_new_session=True)
            for _ in range(100):
                if proc.poll() is not None: raise RuntimeError('Server exited; see verifier-server.log')
                try:
                    Client(base).request('GET','/');return
                except (OSError,urllib.error.URLError):time.sleep(.1)
            raise RuntimeError('Server startup timeout')
        def stop():
            if proc and proc.poll() is None:
                os.killpg(proc.pid,signal.SIGTERM)
                try:proc.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    os.killpg(proc.pid,signal.SIGKILL);proc.wait()
        error=None
        try:
            start(); anon=Client(base);alice=Client(base);bob=Client(base)
            status,body,_=anon.request('GET','/')
            check('HTML entry with browser assets',status==200 and isinstance(body,str) and '<script' in body and '<link' in body)
            check('unauthenticated me',anon.request('GET','/api/me')[0]==401)
            check('unauthenticated mutation',anon.request('POST','/api/tweets',{'text':'no'})[0]==401)
            check('malformed JSON',anon.request('POST','/api/signup',raw=b'{')[0]==400)
            check('invalid signup',anon.request('POST','/api/signup',{'username':'x','password':'short','displayName':'x'})[0]==400)
            credentials={'username':'alice','password':'test-password-123','displayName':'Alice <b>safe</b>'}
            status,body,headers=alice.request('POST','/api/signup',credentials)
            check('signup',status==201 and body.get('user',{}).get('username')=='alice')
            cookie=headers.get('Set-Cookie','')
            check('session cookie flags','httponly' in cookie.lower() and 'samesite' in cookie.lower())
            saved_cookie='; '.join(c.name+'='+c.value for c in alice.jar)
            check('duplicate signup',anon.request('POST','/api/signup',credentials)[0]==409)
            check('bad login',anon.request('POST','/api/login',{'username':'alice','password':'wrong'})[0]==401)
            check('second signup',bob.request('POST','/api/signup',{'username':'bob','password':'test-password-456','displayName':'Bob'})[0]==201)
            for text in ['', '   ', 'x'*281, {'bad':'type'}]:
                check('reject invalid tweet '+repr(text)[:25],alice.request('POST','/api/tweets',{'text':text})[0]==400)
            texts=['first',"<img src=x onerror=alert(1)> '); DROP TABLE users;--",'🦊'*280,'last']
            ids=[]
            for text in texts:
                status,body,_=alice.request('POST','/api/tweets',{'text':text})
                check('create tweet '+str(len(ids)),status==201 and body.get('tweet',{}).get('text')==text)
                ids.append(body['tweet']['id'])
            status,body,_=anon.request('GET','/api/tweets?limit=2')
            page=body.get('tweets',[])
            check('feed newest first and limit',status==200 and [str(t['id']) for t in page]==list(map(str,reversed(ids[-2:]))))
            cursor=body.get('nextCursor')
            check('pagination cursor',cursor is not None)
            status,body,_=anon.request('GET','/api/tweets?limit=2&before='+str(cursor))
            check('pagination exclusive',status==200 and [str(t['id']) for t in body.get('tweets',[])]==list(map(str,reversed(ids[:2]))))
            target='/api/tweets/'+str(ids[-1])+'/like'
            check('like authentication',anon.request('PUT',target)[0]==401)
            check('like first',bob.request('PUT',target)[0]==200)
            check('like idempotent',bob.request('PUT',target)[0]==200)
            tweet=bob.request('GET','/api/tweets?limit=1')[1]['tweets'][0]
            check('like count and viewer state',tweet['likes']==1 and tweet['liked'] is True)
            check('unlike',bob.request('DELETE',target)[0]==200 and bob.request('DELETE',target)[0]==200)
            tweet=bob.request('GET','/api/tweets?limit=1')[1]['tweets'][0]
            check('unlike count and state',tweet['likes']==0 and tweet['liked'] is False)
            check('missing like target',bob.request('PUT','/api/tweets/99999999/like')[0]==404)
            status,body,_=anon.request('GET','/api/users/alice')
            check('profile',status==200 and body.get('user',{}).get('username')=='alice' and len(body.get('tweets',[]))==4)
            status,body,_=anon.request('GET','/api/nonexistent')
            check('API 404 JSON',status==404 and isinstance(body,dict) and isinstance(body.get('error'),str))
            check('logout',alice.request('POST','/api/logout')[0] in [200,204])
            check('session revoked',anon.request('GET','/api/me',headers={'Cookie':saved_cookie})[0]==401)
            check('login after logout',alice.request('POST','/api/login',credentials)[0]==200)
            check('no password API disclosure','password' not in json.dumps(alice.request('GET','/api/me')[1]).lower())
            stop();start()
            check('tweets persist restart',len(anon.request('GET','/api/tweets?limit=50')[1].get('tweets',[]))==4)
            check('users persist restart',alice.request('POST','/api/login',credentials)[0]==200)
            check('SQLite database exists',db.exists() and db.read_bytes()[:16]==b'SQLite format 3\x00')
            conn=sqlite3.connect(db)
            try:
                for (table,) in conn.execute("SELECT name FROM sqlite_master WHERE type='table'"):
                    # Quote metadata-derived table identifiers before inspecting persisted values.
                    for row in conn.execute('SELECT * FROM "'+table.replace('"','""')+'"'):
                        check('no plaintext stored password in '+table,credentials['password'] not in row)
            finally:conn.close()
        except Exception as exc:
            error=type(exc).__name__+': '+str(exc)
        finally:
            stop();log.close()
        report={'passed':error is None,'checks':checks,'error':error,
            'limits':['No automated browser execution: visual quality and runtime DOM XSS are not established.',
                      'Application-owned bun tests are separate evidence; these checks do not invoke them.']}
        write_json(run_dir/'verification.json',report)
        print(json.dumps({'verifier_passed':report['passed'],'checks_passed':sum(c['passed'] for c in checks),'error':error}))
        if error:raise SystemExit(1)


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('action',choices=['prepare','run','verify'])
    parser.add_argument('--run-dir',type=Path)
    parser.add_argument('--hx',default=str(ROOT/'target/release/hx'))
    parser.add_argument('--max-tokens',type=int,default=None)
    parser.add_argument('--max-wall',type=int,default=900)
    parser.add_argument('--reasoning',default='medium')
    args=parser.parse_args()
    if args.action!='prepare' and args.run_dir is None:parser.error('--run-dir is required for run/verify')
    run_dir=(args.run_dir or ROOT/'.harness/rpi-live'/datetime.datetime.now().strftime('%Y%m%d-%H%M%S')).resolve()
    if args.action=='prepare':prepare(run_dir)
    elif args.action=='run':run_agent(run_dir,args)
    else:verify(run_dir)


if __name__=='__main__':main()
