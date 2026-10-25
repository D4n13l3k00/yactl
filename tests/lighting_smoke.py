#!/usr/bin/env python3
"""Exercise stream ownership against simulated hardware only."""
import json
import pathlib
import socket
import subprocess
import sys
import tempfile
import time

binary = str(pathlib.Path(sys.argv[1]).resolve())
with tempfile.TemporaryDirectory(prefix='yactl-lighting-') as tmp:
    root = pathlib.Path(tmp)
    (root/'settings.json').write_text(json.dumps({'http': {'enabled': False}, 'lighting': {'backend': 'simulate'}}))
    worker = subprocess.Popen([binary, '--root', tmp, 'api-worker'], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    def request(path):
        with socket.socket(socket.AF_UNIX) as client:
            client.settimeout(3)
            client.connect(str(root/'api.sock'))
            client.sendall(f'GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n'.encode())
            reply = b''
            while data := client.recv(8192):
                reply += data
        head, body = reply.split(b'\r\n\r\n', 1)
        return int(head.split()[1]), json.loads(body)
    def stream():
        client = socket.socket(socket.AF_UNIX)
        client.settimeout(3)
        client.connect(str(root/'api.sock'))
        client.sendall(b'POST /v1/lighting/stream HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/x-ndjson\r\nTransfer-Encoding: chunked\r\n\r\n')
        return client
    def frame(client, sequence, value=30):
        data = (json.dumps({'sequence':sequence, 'ring':[[value, 0, 0]]*24, 'logo':[[0, value, 0]]*6})+'\n').encode()
        client.sendall(f'{len(data):x}\r\n'.encode()+data+b'\r\n')
    def wait_state(predicate):
        deadline = time.monotonic()+3
        while True:
            code, state = request('/v1/lighting/status')
            assert code == 200, state
            if predicate(state):
                return state
            assert time.monotonic()<deadline, state
            time.sleep(.01)
    try:
        deadline = time.monotonic()+5
        while not (root/'api.sock').exists():
            assert worker.poll() is None, worker.stderr.read().decode()
            assert time.monotonic()<deadline
            time.sleep(.02)
        capabilities = request('/v1/capabilities')[1]
        assert capabilities[0]['simulated'] and capabilities[0]['ring_pixels']==24
        original = request('/v1/lighting/status')[1]['ring']
        with stream() as first:
            frame(first, 1)
            wait_state(lambda s: s['sequence']==1 and s['owned'])
            with stream() as conflict:
                assert b' 409 ' in conflict.recv(8192).split(b'\r\n', 1)[0]
            frame(first, 2, 50)
            state = wait_state(lambda s: s['sequence']==2)
            assert state['ring']==[[50,0,0]]*24
            latency=[]
            for sequence in range(3,23):
                start=time.perf_counter()
                frame(first,sequence,sequence)
                wait_state(lambda s:s['sequence']==sequence)
                latency.append((time.perf_counter()-start)*1000)
        state = wait_state(lambda s: not s['owned'])
        assert state['ring']==original, 'disconnect did not restore captured frame'
        with stream() as second:
            frame(second, 1, 60)
            wait_state(lambda s: s['sequence']==1 and s['owned'])
            frame(second, 1, 70)
            assert b' 409 ' in second.recv(8192).split(b'\r\n', 1)[0]
        wait_state(lambda s: not s['owned'])
        with stream() as idle:
            frame(idle, 1)
            wait_state(lambda s: s['owned'])
            wait_state(lambda s: not s['owned'])
        with stream() as partial:
            wait_state(lambda s: s['owned'])
            start=time.monotonic()
            for _ in range(7):
                partial.sendall(b'1\r\n \r\n')
                time.sleep(.12)
            assert b' 408 ' in partial.recv(8192).split(b'\r\n',1)[0]
            assert time.monotonic()-start<1.5, 'partial chunks reset the frame deadline'
        wait_state(lambda s: not s['owned'])
        ordered=sorted(latency)
        print(f'PASS simulated lighting stream, ownership, sequence, disconnect and expiry; local simulation latency p50={ordered[10]:.1f} ms, p95={ordered[18]:.1f} ms')
    finally:
        if worker.poll() is None:
            worker.terminate()
            try:
                worker.wait(timeout=5)
            except subprocess.TimeoutExpired:
                worker.kill()
                worker.wait()
