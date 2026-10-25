#!/usr/bin/env python3
"""Exercise real process supervision in an isolated temporary directory."""
import argparse,json,os,pathlib,signal,socket,subprocess,sys,tempfile,threading,time
p=argparse.ArgumentParser();p.add_argument('--binary',default='target/debug/yactl');a=p.parse_args()
binary=str(pathlib.Path(a.binary).resolve())
def dead(pid):
    try:return pathlib.Path(f'/proc/{pid}/stat').read_text().rsplit(')',1)[1].split()[0]=='Z'
    except FileNotFoundError:return True
with tempfile.TemporaryDirectory(prefix='yactl-smoke-') as tmp:
    base=pathlib.Path(tmp);root=base/'state';cwd=base/'work space';cwd.mkdir()
    root.mkdir();(root/'settings.json').write_text(json.dumps({'http':{'enabled':False}}))
    output=open(base/'manager-output.log','wb');manager=None
    def cli(*args,ok=True,raw=False):
        r=subprocess.run([binary,'--root',str(root),'--json',*map(str,args)],capture_output=True,text=True,timeout=40)
        if ok and r.returncode:raise AssertionError((args,r.stdout,r.stderr))
        return r.stdout if raw else (json.loads(r.stdout) if r.returncode==0 and r.stdout.strip() else r)
    def status(name):return cli('status',name)['status']
    def wait(pred,timeout=5):
        end=time.monotonic()+timeout
        while time.monotonic()<end:
            value=pred()
            if value:return value
            time.sleep(.03)
        raise AssertionError('condition timed out')
    def start():
        global manager
        manager=subprocess.Popen([binary,'--root',str(root),'daemon'],stdout=output,stderr=output)
        wait(lambda:(root/'control.sock').exists() and cli('list',ok=False).__class__ is dict)
    try:
        start();original_pid=manager.pid
        assert cli('daemon','--detach')['pid']==original_pid
        duplicate=cli('daemon',ok=False);assert duplicate.returncode!=0
        assert (root/'control.sock').stat().st_mode&0o777==0o600
        literal='literal $HOME; touch SHOULD_NOT_EXIST'
        code="import os,sys,time;print(os.getcwd()+'|'+os.environ['HELLO']+'|'+sys.argv[1],flush=True);print('STDERR_PROBE',file=sys.stderr,flush=True);time.sleep(120)"
        cli('add','echo','--cwd',cwd,'--env','HELLO=world','--',sys.executable,'-c',code,literal)
        wait(lambda:'STDERR_PROBE' in (root/'logs/echo.log').read_text())
        log=(root/'logs/echo.log').read_text();assert f'{cwd}|world|{literal}' in log and not (cwd/'SHOULD_NOT_EXIST').exists()
        first=status('echo')['pid'];cli('restart','echo');assert status('echo')['pid']!=first
        cli('stop','echo');assert status('echo')['pid'] is None;time.sleep(.15);assert status('echo')['state']=='stopped'
        cli('remove','echo');assert (root/'logs/echo.log').exists()
        print('PASS argv/env/cwd, output, restart/stop, config removal',flush=True)

        cli('add','crash','--restart-delay-ms','100','--','/bin/sh','-c','exit 7')
        wait(lambda:status('crash')['restart_count']>=2)
        s=status('crash');assert s['last_exit']==7 and s['failures']>=2
        cli('stop','crash');count=status('crash')['restart_count'];time.sleep(.25);assert status('crash')['restart_count']==count
        cli('remove','crash')
        cli('add','once','--restart','never','--','/bin/true');wait(lambda:status('once')['state']=='exited');cli('remove','once')
        cli('add','log','--restart','never','--log-max-kib','4','--',sys.executable,'-c',"for i in range(3000): print(str(i)+'x'*100)")
        wait(lambda:status('log')['state']=='exited');assert (root/'logs/log.log').stat().st_size<=4096 and (root/'logs/log.log.1').stat().st_size<=4096
        assert '2999' in cli('logs','log','-n','3',raw=True);cli('remove','log')
        print('PASS failure backoff, no restart policy, bounded log rotation',flush=True)

        worker=base/'worker.pid'
        child_code=f"import os,signal,time,pathlib;signal.signal(signal.SIGTERM,signal.SIG_IGN);pathlib.Path({str(worker)!r}).write_text(str(os.getpid()));time.sleep(120)"
        parent_code=f"import signal,subprocess,sys,time;signal.signal(signal.SIGTERM,signal.SIG_IGN);subprocess.Popen([sys.executable,'-c',{child_code!r}]);time.sleep(120)"
        cli('add','tree','--stop-timeout-ms','200','--',sys.executable,'-c',parent_code)
        wait(worker.exists);wpid=int(worker.read_text());leader=status('tree')['pid'];t=time.monotonic();cli('stop','tree')
        assert .15<time.monotonic()-t<3;wait(lambda:dead(wpid) and dead(leader));wait(lambda:not pathlib.Path(f'/proc/{wpid}').exists());cli('remove','tree')
        worker.unlink()
        parent_code=f"import subprocess,sys,time,pathlib;subprocess.Popen([sys.executable,'-c',{child_code!r}]);p=pathlib.Path({str(worker)!r});\nwhile not p.exists():time.sleep(.01)"
        cli('add','orphan','--restart','never','--',sys.executable,'-c',parent_code)
        wait(worker.exists);wpid=int(worker.read_text());wait(lambda:status('orphan')['state']=='exited');wait(lambda:dead(wpid));wait(lambda:not pathlib.Path(f'/proc/{wpid}').exists());cli('remove','orphan')
        print('PASS TERM-resistant process group, leader exit, orphan reaping',flush=True)

        cli('add','disabled','--disabled','--','/bin/sleep','120');cli('reload','disabled');assert status('disabled')['pid'] is None
        cli('enable','disabled','--now');assert status('disabled')['pid']
        cli('disable','disabled','--now');cli('reload','disabled');assert status('disabled')['pid'] is None
        cli('add','boot','--','/bin/sleep','120');cli('stop','boot');assert status('boot')['enabled']
        # A slow partial request must not monopolize the single supervisor loop.
        def slow():
            with socket.socket(socket.AF_UNIX) as s:
                s.connect(str(root/'control.sock'))
                for _ in range(8):
                    try:s.sendall(b' ')
                    except OSError:break
                    time.sleep(.15)
        t=threading.Thread(target=slow);t.start();time.sleep(.04);before=time.monotonic();cli('list');assert time.monotonic()-before<1;t.join()
        (root/'services/broken.json').write_text('{invalid')
        cli('shutdown');manager.wait(timeout=5);assert not (root/'control.sock').exists()
        start();assert status('boot')['pid'];assert status('disabled')['pid'] is None
        assert any(i['name']=='broken' for i in cli('list')['invalid']);cli('remove','broken')
        print('PASS disabled reload, private IPC deadline, persistence and bad config isolation',flush=True)

        child=status('boot')['pid'];manager.kill();manager.wait(timeout=5);wait(lambda:dead(child))
        start();assert status('boot')['pid']!=child
        cli('disable','boot','--now');cli('remove','boot');cli('remove','disabled');cli('shutdown');manager.wait(timeout=5)
        print('PASS abrupt manager death, parent-death signal, stale socket recovery, graceful shutdown',flush=True)
    finally:
        if manager is not None and manager.poll() is None:
            try:cli('shutdown')
            except Exception:manager.terminate()
            try:manager.wait(timeout=5)
            except subprocess.TimeoutExpired:manager.kill();manager.wait()
        output.close()
