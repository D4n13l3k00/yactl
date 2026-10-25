#!/usr/bin/env python3
"""API shutdown is bounded even when a blocking Unix connect cannot finish."""
import json
import pathlib
import signal
import socket
import subprocess
import sys
import tempfile
import time

binary = str(pathlib.Path(sys.argv[1]).resolve())
with tempfile.TemporaryDirectory(prefix='yactl-api-shutdown-') as tmp:
    root = pathlib.Path(tmp)
    (root/'settings.json').write_text(json.dumps({'http': {'enabled': False}}))
    worker = subprocess.Popen([binary, '--root', tmp, 'api-worker'], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    clients = []
    try:
        deadline = time.monotonic()+5
        while not (root/'api.sock').exists():
            assert worker.poll() is None, worker.stderr.read().decode()
            assert time.monotonic() < deadline, 'API startup timeout'
            time.sleep(.02)
        with socket.socket(socket.AF_UNIX) as stalled:
            stalled.bind(str(root/'control.sock'))
            stalled.listen(0)
            # Fill the backlog so forwarded connects block before their read deadline.
            with socket.socket(socket.AF_UNIX) as backlog:
                backlog.connect(str(root/'control.sock'))
                for _ in range(4):
                    client = socket.socket(socket.AF_UNIX)
                    client.settimeout(4)
                    client.connect(str(root/'api.sock'))
                    client.sendall(b'POST /v1/services/request HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: 17\r\n\r\n{"action":"ping"}')
                    clients.append(client)
                # Obtain an explicit busy response before testing shutdown.
                deadline = time.monotonic()+2
                while True:
                    with socket.socket(socket.AF_UNIX) as probe:
                        probe.settimeout(4)
                        probe.connect(str(root/'api.sock'))
                        probe.sendall(b'POST /v1/services/request HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: 17\r\n\r\n{"action":"ping"}')
                        response = probe.recv(4096)
                    if b' 503 ' in response.split(b'\r\n', 1)[0]:
                        break
                    assert time.monotonic() < deadline, 'forwarding was not saturated'
                start = time.monotonic()
                worker.send_signal(signal.SIGTERM)
                worker.wait(timeout=3)
                assert worker.returncode == 0 and time.monotonic()-start < 2, 'API shutdown stalled'
        print('PASS bounded API shutdown during blocked supervisor connect')
    finally:
        for client in clients:
            client.close()
        if worker.poll() is None:
            worker.kill()
            worker.wait(timeout=5)
