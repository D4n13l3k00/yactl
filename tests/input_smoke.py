#!/usr/bin/env python3
"""Simulated key/switch events, cached state, authorization and journal overflow."""
import http.client
import json
import pathlib
import socket
import subprocess
import sys
import tempfile
import time

binary = str(pathlib.Path(sys.argv[1]).resolve())
with tempfile.TemporaryDirectory(prefix='yactl-input-') as tmp:
    root = pathlib.Path(tmp)
    keys = []
    for name, scopes in [('reader', ['read']), ('hardware', ['hardware'])]:
        key = root/(name+'.token')
        key.write_text(name[0]*32)
        key.chmod(0o600)
        keys.append({'file':str(key),'scopes':scopes})
    config = {'http':{'listen':'127.0.0.1:0'},'keys':keys,'input':{'backend':'simulate'}}
    (root/'settings.json').write_text(json.dumps(config))
    worker = subprocess.Popen([binary,'--root',tmp,'api-worker'],stdout=subprocess.DEVNULL,stderr=subprocess.PIPE)
    def request(path, method='GET', address=None, token=None):
        if address:
            client = http.client.HTTPConnection(address,timeout=4)
        else:
            client = http.client.HTTPConnection('localhost',timeout=4)
            client.sock = socket.socket(socket.AF_UNIX)
            client.sock.settimeout(4)
            client.sock.connect(str(root/'api.sock'))
        try:
            client.request(method,path,headers={'Authorization':'Bearer '+token} if token else {})
            response = client.getresponse()
            return response.status,json.loads(response.read())
        finally:
            client.close()
    try:
        deadline = time.monotonic()+5
        while not (root/'api.sock').exists():
            assert worker.poll() is None,worker.stderr.read().decode()
            assert time.monotonic()<deadline
            time.sleep(.01)
        while True:
            code,status = request('/v1/input')
            assert code == 200,status
            if status['ready']:
                break
            assert time.monotonic()<deadline
            time.sleep(.01)
        assert status['supported'] and status['simulated']
        device = status['devices'][0]
        assert device['id']=='input:simulated' and device['keys']==[115] and device['switches']==[14]
        capability = request('/v1/capabilities')[1][0]
        assert capability['id']=='input.events.v1' and capability['simulated'] and capability['ready']
        page = request('/v1/events')[1]
        stream = page['stream_id']
        seen = page['events']
        cursor = page['cursor']
        deadline = time.monotonic()+3
        while not all(any(e['kind']==kind for e in seen) for kind in ('input_snapshot','key','switch')):
            assert time.monotonic()<deadline
            page = request(f'/v1/events?stream_id={stream}&after={cursor}&wait_ms=500')[1]
            seen.extend(page['events'])
            cursor = page['cursor']
        assert all(e['source']=='input:simulated' for e in seen)
        key = next(e for e in seen if e['kind']=='key')
        switch = next(e for e in seen if e['kind']=='switch')
        assert key['data']=={'type':1,'code':115,'value':1}
        assert switch['data']=={'type':5,'code':14,'value':1}
        address = request('/v1/health')[1]['http']
        assert request('/v1/input',address=address,token='r'*32)[0]==200
        assert request('/v1/input',address=address,token='h'*32)[0]==403
        assert request('/v1/input','POST')[0]==405
        changed = {**config,'input':{'backend':'off'}}
        (root/'settings.json').write_text(json.dumps(changed))
        code,failure = request('/v1/config/reload','POST')
        assert code==400 and 'restart' in failure['error']['message']
        assert request('/v1/input')[1]['ready']
        deadline = time.monotonic()+8
        while request('/v1/events')[1]['latest']<90:
            assert time.monotonic()<deadline
            time.sleep(.1)
        page = request(f'/v1/events?stream_id={stream}&after=0&limit=2')[1]
        assert page['lost']>0 and len(page['events'])==2
        assert page['lost']==page['events'][0]['sequence']-1
        assert page['cursor']==page['events'][-1]['sequence']<page['latest']
        cached = request('/v1/input')[1]['devices'][0]['state']
        assert cached in ({'keys_down':[],'switches_on':[]},{'keys_down':[115],'switches_on':[14]})
        start = time.monotonic()
        worker.terminate()
        worker.wait(timeout=3)
        assert time.monotonic()-start<2
        print('PASS input simulation, raw codes, cached state, read scope, loss and bounded shutdown')
    finally:
        if worker.poll() is None:
            worker.terminate()
            try:worker.wait(timeout=4)
            except subprocess.TimeoutExpired:worker.kill();worker.wait()
