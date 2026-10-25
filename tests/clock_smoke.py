#!/usr/bin/env python3
"""Clock ownership and validation use a simulated provider, never live sysfs."""
import json
import pathlib
import socket
import subprocess
import sys
import tempfile
import time

binary = str(pathlib.Path(sys.argv[1]).resolve())
with tempfile.TemporaryDirectory(prefix='yactl-clock-') as tmp:
    root = pathlib.Path(tmp)
    config = {'http': {'enabled': False}, 'clock': {'backend': 'simulate'}, 'lighting': {'backend': 'simulate'}}
    (root/'settings.json').write_text(json.dumps(config))
    worker = subprocess.Popen([binary, '--root', tmp, 'api-worker'], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    def request(path, method='GET', body=b''):
        with socket.socket(socket.AF_UNIX) as client:
            client.settimeout(3);client.connect(str(root/'api.sock'))
            client.sendall(f'{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {len(body)}\r\n\r\n'.encode()+body)
            reply=b''
            while data:=client.recv(8192):reply+=data
        head,body=reply.split(b'\r\n\r\n',1)
        return int(head.split()[1]),json.loads(body)
    def stream(domain='clock'):
        client=socket.socket(socket.AF_UNIX);client.settimeout(3);client.connect(str(root/'api.sock'))
        client.sendall(f'POST /v1/{domain}/stream HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/x-ndjson\r\nTransfer-Encoding: chunked\r\n\r\n'.encode())
        return client
    def send(client, body):
        data=(json.dumps(body)+'\n').encode();client.sendall(f'{len(data):x}\r\n'.encode()+data+b'\r\n')
    def wait_state(predicate, domain='clock'):
        end=time.monotonic()+3
        while True:
            code,state=request(f'/v1/{domain}/status');assert code==200,state
            if predicate(state):return state
            assert time.monotonic()<end,state
            time.sleep(.01)
    try:
        end=time.monotonic()+5
        while not (root/'api.sock').exists():
            assert worker.poll() is None,worker.stderr.read().decode()
            assert time.monotonic()<end
            time.sleep(.02)
        caps=request('/v1/capabilities')[1]
        assert {c['id'] for c in caps}=={'lighting.rgb.v1','display.clock.segment.v1'}
        original=request('/v1/clock/status')[1]['segments']
        assert len(original)==45 and all(v==0 for v in original.values())
        with stream() as first:
            send(first,{'sequence':1,'time':'12:34','brightness':60})
            state=wait_state(lambda s:s['sequence']==1)
            assert state['segments']['b1']==60 and state['segments']['a1']==0
            assert state['segments']['p3']==60 and state['segments']['q3']==60
            with stream() as conflict:
                assert b' 409 ' in conflict.recv(8192).split(b'\r\n',1)[0]
            with stream('lighting') as rgb:
                send(rgb,{'sequence':1,'ring':[[1,2,3]]*24,'logo':[[4,5,6]]*6})
                wait_state(lambda s:s['owned'], 'lighting')
                send(first,{'sequence':2,'segments':{'a1':120}})
                state=wait_state(lambda s:s['sequence']==2)
                assert state['segments']['a1']==120 and state['segments']['b1']==0
                assert request('/v1/lighting/status')[1]['owned'], 'clock stole RGB ownership'
        assert wait_state(lambda s:not s['owned'])['segments']==original
        for invalid in [
            {'sequence':1,'time':'24:00'},
            {'sequence':1,'time':'12:60'},
            {'sequence':1,'time':'1:23'},
            {'sequence':1,'time':'12:00','brightness':256},
            {'sequence':1,'time':'12:00','segments':{}},
            {'sequence':1,'segments':{'../brightness':1}},
            {'sequence':1,'segments':{'unknown':1}},
        ]:
            with stream() as client:
                send(client,invalid)
                assert b' 400 ' in client.recv(8192).split(b'\r\n',1)[0],invalid
            assert wait_state(lambda s:not s['owned'])['segments']==original
        with stream() as idle:
            send(idle,{'sequence':1,'time':'23:59'})
            wait_state(lambda s:s['owned'])
            assert wait_state(lambda s:not s['owned'])['segments']==original
        changed={**config,'clock':{'backend':'off'}}
        (root/'settings.json').write_text(json.dumps(changed))
        code,error=request('/v1/config/reload','POST',b'{}')
        assert code==400 and 'restart' in error['error']['message']
        assert request('/v1/clock/status')[1]['ready']
        print('PASS simulated clock time/segments, independent ownership, validation, release and expiry')
    finally:
        if worker.poll() is None:
            worker.terminate()
            try:worker.wait(timeout=5)
            except subprocess.TimeoutExpired:worker.kill();worker.wait()
