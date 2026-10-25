#!/usr/bin/env python3
"""Use local synthetic firmware peers; never connect to a station."""
import http.client
import json
import pathlib
import socket
import struct
import subprocess
import sys
import tempfile
import time

binary = str(pathlib.Path(sys.argv[1]).resolve())
speech = bytes([0x1a, 4, 0x12, 2, ord('h'), ord('i')])
alarm = bytes([0xba, 0x0d, 19, 0x0a, 17, 0x0a, 1, ord('a'), 0x10, 3, 0x18, 0x96, 1, 0x20, 60, 0x2a, 3, ord('t'), ord('e'), ord('a'), 0x30, 1])

with tempfile.TemporaryDirectory(prefix='yactl-firmware-') as tmp:
    root = pathlib.Path(tmp)
    peers = []
    listeners = []
    for _ in range(2):
        listener = socket.socket()
        listener.bind(('127.0.0.1', 0))
        listener.listen(4)
        listener.settimeout(4)
        listeners.append(listener)
    config = {'http': {'enabled': False}, 'firmware': dict(zip(['aliced', 'alarmd'], ['127.0.0.1:'+str(s.getsockname()[1]) for s in listeners]))}
    (root/'settings.json').write_text(json.dumps(config))
    worker = subprocess.Popen([binary, '--root', tmp, 'api-worker'], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)

    def request(path, method='GET'):
        client = http.client.HTTPConnection('localhost', timeout=4)
        client.sock = socket.socket(socket.AF_UNIX)
        client.sock.settimeout(4)
        client.sock.connect(str(root/'api.sock'))
        try:
            client.request(method, path)
            response = client.getresponse()
            return response.status, json.loads(response.read())
        finally:
            client.close()

    def wait_for(predicate, timeout=4):
        deadline = time.monotonic() + timeout
        while True:
            result = predicate()
            if result:
                return result
            assert worker.poll() is None, worker.stderr.read().decode()
            assert time.monotonic() < deadline, 'condition timed out'
            time.sleep(.01)

    def state(service):
        providers = request('/v1/health')[1]['providers']
        return next(p for p in providers if p['id'] == 'events.firmware.'+service+'.v1')

    def send(peer, payload):
        peer.sendall(struct.pack('<I', len(payload)) + payload)

    try:
        wait_for(lambda: (root/'api.sock').exists())
        for listener in listeners:
            peer, _ = listener.accept()
            peer.settimeout(.05)
            peers.append(peer)
            try:
                assert not peer.recv(1), 'subscription sent a protocol command'
                raise AssertionError('subscription disconnected before data')
            except socket.timeout:
                pass
        assert request('/v1/capabilities') == (200, [])
        assert not state('aliced')['supported'] and not state('alarmd')['supported']
        send(peers[0], bytes([8, 1]))  # valid but unrelated envelope
        assert not state('aliced')['supported']
        initial = request('/v1/events')[1]
        stream = initial['stream_id']
        peers[0].sendall(bytes([len(speech)]))
        peers[0].sendall(bytes([0, 0, 0]) + speech[:2])
        peers[0].sendall(speech[2:])
        send(peers[1], alarm)
        wait_for(lambda: state('aliced')['ready'] and state('alarmd')['ready'])
        batch = request('/v1/events')[1]
        events = {e['kind']: e for e in batch['events']}
        assert events['speech']['data'] == {'source': 'voice', 'transcript': 'hi'}
        assert events['alarm_fired']['data']['id'] == 'a'
        assert events['alarm_fired']['data']['start_timestamp_ms'] == 150
        # Both formats share the same bounded subscription and journal. Only
        # final command-spotter requests are exposed as command speech.
        cursor = batch['latest']
        published = state('aliced')['published_events']
        send(peers[0], bytes([0xe2,0x0f,4,0x12,2,ord('h'),ord('i'),0xe8,0x0f,3]))
        command = bytearray([0xe2,0x0f,21,0x52,9,0x32,7,*b'command',0x7a,8,0x0a,4,*b'stop',0x10,1,0xe8,0x0f,3])
        send(peers[0], command)
        command[23] = 0
        send(peers[0], command)
        send(peers[0], bytes([0x1a,5,0x12,3,*b'end']))
        wait_for(lambda: state('aliced')['published_events'] >= published+3)
        newer = request(f'/v1/events?stream_id={stream}&after={cursor}')[1]['events']
        assert [e['data'] for e in newer if e['kind']=='speech'] == [
            {'source':'voice','transcript':'hi'},
            {'source':'command','transcript':'stop'},
            {'source':'voice','transcript':'end'},
        ]
        caps = request('/v1/capabilities')[1]
        assert {c['id'] for c in caps} == {'events.firmware.aliced.v1', 'events.firmware.alarmd.v1'}
        # Valid protobuf can exceed the journal bound after JSON escaping.
        send(peers[0], bytes([0x1a, 0x83, 2, 0x12, 0x80, 2]) + b'\x01'*256)
        wait_for(lambda: state('aliced')['dropped_events'] == 1)
        assert state('aliced')['last_error'] == 'event_too_large'
        assert any(e['kind'] == 'source_loss' and e['data']['reason'] == 'event_too_large' for e in request('/v1/events')[1]['events'])
        # Malformed input disconnects only its own source and reports loss.
        send(peers[0], b'\x00')
        wait_for(lambda: state('aliced')['last_error'] == 'invalid_frame')
        assert state('aliced')['supported'] and not state('aliced')['ready']
        assert state('alarmd')['ready']
        replacement, _ = listeners[0].accept()
        peers.append(replacement)
        assert not state('aliced')['ready']
        send(replacement, speech)
        wait_for(lambda: state('aliced')['ready'])
        assert request('/v1/events')[1]['stream_id'] == stream
        # Refuse oversized frames before reading/allocating the declared body.
        replacement.sendall(struct.pack('<I', 65537))
        wait_for(lambda: state('aliced')['last_error'] == 'oversized_frame')
        partial, _ = listeners[0].accept()
        peers.append(partial)
        # Idle time before the first byte is not an incomplete-frame timeout.
        time.sleep(2.1)
        assert state('aliced')['connected']
        start = time.monotonic()
        partial.sendall(b'\x06')
        time.sleep(.7)
        partial.sendall(b'\x00')
        time.sleep(.7)
        partial.sendall(b'\x00')
        wait_for(lambda: state('aliced')['last_error'] == 'frame_timeout', timeout=1.2)
        assert time.monotonic() - start < 2.8
        changed = {**config, 'firmware': {}}
        (root/'settings.json').write_text(json.dumps(changed))
        code, failure = request('/v1/config/reload', 'POST')
        assert code == 400 and 'restart' in failure['error']['message']
        assert state('alarmd')['ready']
        (root/'settings.json').write_text(json.dumps(config))
        assert request('/v1/config/reload', 'POST')[0] == 200
        idle, _ = listeners[0].accept()
        peers.append(idle)
        start = time.monotonic()
        worker.terminate()
        worker.wait(timeout=3)
        assert time.monotonic() - start < 2
        print('PASS firmware events, bounded decoding, loss, reconnect, deadlines and restart-only settings')
    finally:
        if worker.poll() is None:
            worker.terminate()
            try:
                worker.wait(timeout=4)
            except subprocess.TimeoutExpired:
                worker.kill()
                worker.wait()
        for peer in peers:
            peer.close()
        for listener in listeners:
            listener.close()
