#!/usr/bin/env python3
"""Independent midi-light must render through yactl's simulated stream API."""
import json
import pathlib
import socket
import subprocess
import sys
import tempfile
import time
import urllib.request

binary, midi = (str(pathlib.Path(arg).resolve()) for arg in sys.argv[1:3])
with tempfile.TemporaryDirectory(prefix='yactl-midi-client-') as tmp:
    root=pathlib.Path(tmp)
    (root/'settings.json').write_text(json.dumps({'http':{'enabled':False}, 'lighting':{'backend':'simulate'}}))
    api=subprocess.Popen([binary,'--root',tmp,'api-worker'],stdout=subprocess.DEVNULL,stderr=subprocess.PIPE)
    client=None
    def provider():
        with socket.socket(socket.AF_UNIX) as s:
            s.settimeout(2);s.connect(str(root/'api.sock'))
            s.sendall(b'GET /v1/lighting/status HTTP/1.1\r\nHost: localhost\r\n\r\n')
            response=b''
            while data:=s.recv(8192):response+=data
        return json.loads(response.split(b'\r\n\r\n',1)[1])
    def wait(predicate):
        deadline=time.monotonic()+4
        while not predicate():
            assert time.monotonic()<deadline, 'client/provider state timeout'
            time.sleep(.02)
    try:
        wait(lambda: (root/'api.sock').exists() or api.poll() is not None)
        assert api.poll() is None, api.stderr.read().decode()
        with socket.socket() as free:
            free.bind(('127.0.0.1',0));port=free.getsockname()[1]
        address=f'127.0.0.1:{port}'
        client=subprocess.Popen([midi,'serve','--listen',address,'--no-openrgb','--yactl-socket',str(root/'api.sock')],stdout=subprocess.DEVNULL,stderr=subprocess.PIPE)
        def http(body=None):
            req=urllib.request.Request(f'http://{address}/api/state' if body is None else f'http://{address}/api/control',data=None if body is None else json.dumps(body).encode(),headers={'Content-Type':'application/json'})
            with urllib.request.urlopen(req,timeout=2) as response:return json.load(response)
        def started():
            assert client.poll() is None,client.stderr.read().decode()
            try:return http() is not None
            except OSError:return False
        wait(started)
        http({'mode':'solid','color':'ff0000','logo_color':'00ff00','logo_follow':False,'brightness':.2,'active':True})
        wait(lambda: provider()['ring'][0]==[51,0,0])
        assert provider()['logo'][0]==[0,51,0]
        assert not http()['error']
        # A connection must survive beyond the ordinary six-second HTTP deadline.
        time.sleep(6.2)
        assert provider()['owned'] and not http()['error']
        http({'active':False})
        wait(lambda:not provider()['owned'])
        assert provider()['ring']==[[0,0,0]]*24
        http({'active':True})
        wait(lambda:provider()['owned'])
        api.kill();api.wait(timeout=3)
        wait(lambda:not http()['active'] and bool(http()['error']))
        api=subprocess.Popen([binary,'--root',tmp,'api-worker'],stdout=subprocess.DEVNULL,stderr=subprocess.PIPE)
        def provider_ready():
            try:return provider()['ready']
            except OSError:return False
        wait(provider_ready)
        http({'active':True})
        wait(lambda:provider()['owned'])
        client.kill();client.wait(timeout=3)
        wait(lambda:not provider()['owned'])
        assert provider()['ring']==[[0,0,0]]*24
        print('PASS independent midi-light RGB parity, long stream, release, provider failure and client crash restoration')
    finally:
        for process in (client,api):
            if process is not None and process.poll() is None:
                process.terminate()
                try:process.wait(timeout=4)
                except subprocess.TimeoutExpired:process.kill();process.wait()
