#!/usr/bin/env python3
"""Slow/overloaded control clients must not stall process supervision."""
import json, os, pathlib, signal, socket, subprocess, sys, tempfile, time
binary = str(pathlib.Path(sys.argv[1]).resolve())
with tempfile.TemporaryDirectory(prefix='yactl-async-') as tmp:
    root = pathlib.Path(tmp)
    (root/'settings.json').write_text(json.dumps({'http':{'enabled':False}}))
    manager = subprocess.Popen([binary, '--root', tmp, 'daemon'], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    clients = []
    def cli(*args):
        result = subprocess.run([binary, '--root', tmp, '--json', *args], capture_output=True, text=True, timeout=5)
        assert result.returncode == 0, result.stderr
        return json.loads(result.stdout)
    try:
        end=time.monotonic()+5
        while not (root/'control.sock').exists():
            assert manager.poll() is None, manager.stderr.read().decode()
            assert time.monotonic()<end
            time.sleep(.02)
        cli('add','crash','--restart-delay-ms','100','--','/bin/sh','-c','echo restarted; exit 7')
        for _ in range(8):
            s=socket.socket(socket.AF_UNIX); s.settimeout(1)
            s.connect(str(root/'control.sock')); s.sendall(b' '); clients.append(s)
        # Total request deadlines run concurrently, not eight sequential 250 ms waits.
        time.sleep(.4)
        start=time.monotonic(); info=cli('status','crash')
        assert time.monotonic()-start<.5, 'stalled clients serialized IPC handling'
        assert info['status']['restart_count'] >= 1, 'supervision starved'
        cli('stop','crash')
        end=time.monotonic()+5
        while True:
            try:
                if cli('health')['supervisor']['ready']: break
            except AssertionError:
                pass
            assert time.monotonic()<end, 'supervisor health never became ready'
            time.sleep(.04)
        api=cli('status','yactl-api')['status']['pid']
        assert api, 'API worker not supervised'
        assert pathlib.Path(f'/proc/{api}').exists(), 'API PID missing'
        os.kill(api,signal.SIGKILL)
        end=time.monotonic()+5
        while True:
            state=cli('status','yactl-api')['status']
            if state['pid'] and state['pid']!=api: break
            assert time.monotonic()<end, 'API worker was not restarted'
            time.sleep(.04)
        for s in clients: s.close()
        clients=[]
        for _ in range(8):
            s=socket.socket(socket.AF_UNIX); s.settimeout(1)
            s.connect(str(root/'control.sock')); s.sendall(b' '); clients.append(s)
        start=time.monotonic(); manager.send_signal(signal.SIGTERM); manager.wait(timeout=5)
        assert manager.returncode==0 and time.monotonic()-start<2, 'signal shutdown stalled'
        print('PASS concurrent deadlines, supervision under slow clients and bounded signal shutdown')
    finally:
        for s in clients: s.close()
        if manager.poll() is None:
            manager.terminate()
            try: manager.wait(timeout=5)
            except subprocess.TimeoutExpired: manager.kill(); manager.wait()
