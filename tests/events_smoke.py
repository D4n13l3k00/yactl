#!/usr/bin/env python3
"""Exercise event cursor validation, authorization and restart identity."""
import http.client
import json
import pathlib
import socket
import subprocess
import sys
import tempfile
import time
from concurrent.futures import ThreadPoolExecutor

binary = str(pathlib.Path(sys.argv[1]).resolve())
with tempfile.TemporaryDirectory(prefix='yactl-events-') as tmp:
    root = pathlib.Path(tmp)
    keys = []
    for name, scopes in [('reader', ['read']), ('hardware', ['hardware'])]:
        key = root / (name + '.token')
        key.write_text(name[0] * 32)
        key.chmod(0o600)
        keys.append({'file': str(key), 'scopes': scopes})
    (root / 'settings.json').write_text(json.dumps({'http': {'listen': '127.0.0.1:0'}, 'keys': keys}))

    def start():
        worker = subprocess.Popen([binary, '--root', tmp, 'api-worker'], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        deadline = time.monotonic() + 5
        while not (root / 'api.sock').exists():
            assert worker.poll() is None, worker.stderr.read().decode()
            assert time.monotonic() < deadline
            time.sleep(.02)
        return worker

    def request(path, method='GET', token=None, address=None):
        if address:
            client = http.client.HTTPConnection(address, timeout=4)
        else:
            client = http.client.HTTPConnection('localhost', timeout=4)
            client.sock = socket.socket(socket.AF_UNIX)
            client.sock.settimeout(4)
            client.sock.connect(str(root / 'api.sock'))
        try:
            headers = {'Authorization': 'Bearer ' + token} if token else {}
            client.request(method, path, headers=headers)
            response = client.getresponse()
            if path.startswith('/v1/events') and response.status in (200, 400, 409):
                assert response.getheader('Cache-Control') == 'no-store'
            return response.status, json.loads(response.read())
        finally:
            client.close()

    worker = start()
    try:
        code, first = request('/v1/events')
        assert code == 200, first
        stream = first['stream_id']
        assert len(stream) == 32 and all(c in '0123456789abcdef' for c in stream)
        assert first['events'] == [] and first['cursor'] == first['latest'] == first['lost'] == 0
        assert not first['closed']
        cursor = '?stream_id=' + stream + '&after=0'
        for query in [
            '?after=0', '?stream_id=' + stream, '?after=-1&stream_id=' + stream,
            cursor + '&after=0', cursor + '&wait_ms=2001', cursor + '&wait_ms=-1',
            cursor + '&limit=0', cursor + '&limit=17', cursor + '&unknown=1',
            cursor + '&limit=1&limit=2', cursor + '&wait_ms=%31', '?limit=+1',
            '?after=18446744073709551616&stream_id=' + stream,
        ]:
            code, failure = request('/v1/events' + query)
            assert code == 400 and failure['error']['code'] == 'invalid_request', (query, code, failure)
        assert request('/v1/events?stream_id=' + stream + '&after=1')[0] == 409
        code, reset = request('/v1/events?stream_id=' + '0' * 32 + '&after=0')
        assert code == 409 and reset['error']['code'] == 'event_stream_reset'
        assert reset['stream_id'] == stream
        address = request('/v1/health')[1]['http']
        assert request('/v1/events', token='r'*32, address=address)[0] == 200
        assert request('/v1/events', token='h'*32, address=address)[0] == 403
        assert request('/v1/events', token='wrong', address=address)[0] == 401
        assert request('/v1/events', method='POST')[0] == 405
        # Pending event readers must not block health or configuration reload.
        with ThreadPoolExecutor(max_workers=8) as pool:
            pending = [pool.submit(request, '/v1/events' + cursor + '&wait_ms=300') for _ in range(8)]
            start_time = time.monotonic()
            assert request('/v1/health')[0] == 200
            assert request('/v1/config/reload', method='POST')[0] == 200
            assert time.monotonic() - start_time < 1
            for result in pending:
                code, batch = result.result(timeout=2)
                assert code == 200 and not batch['events'] and batch['stream_id'] == stream
        assert request('/v1/events')[1]['stream_id'] == stream
        identity = request('/v1/device')[1]['instance_id']
        worker.terminate()
        worker.wait(timeout=4)
        worker = start()
        assert request('/v1/device')[1]['instance_id'] == identity
        new_stream = request('/v1/events')[1]['stream_id']
        assert new_stream != stream
        code, reset = request('/v1/events' + cursor)
        assert code == 409 and reset['error']['code'] == 'event_stream_reset'
        assert reset['stream_id'] == new_stream
        assert request('/v1/capabilities') == (200, [])
        print('PASS event reads, cursor validation, read scope, bounded polling and restart identity')
    finally:
        if worker.poll() is None:
            worker.terminate()
            try:
                worker.wait(timeout=4)
            except subprocess.TimeoutExpired:
                worker.kill()
                worker.wait()
