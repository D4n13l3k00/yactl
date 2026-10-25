#!/usr/bin/env python3
"""Exercise read-only sensor discovery and cached readings in simulation."""
import json
import pathlib
import socket
import subprocess
import sys
import tempfile
import time
import urllib.request

binary=str(pathlib.Path(sys.argv[1]).resolve())
with tempfile.TemporaryDirectory(prefix='yactl-sensors-') as tmp:
    root=pathlib.Path(tmp)
    key=root/'reader.token';key.write_text('r'*32);key.chmod(0o600)
    config={'http':{'listen':'127.0.0.1:0'},'keys':[{'file':str(key),'scopes':['read']}],'sensors':{'backend':'simulate'}}
    (root/'settings.json').write_text(json.dumps(config))
    worker=subprocess.Popen([binary,'--root',tmp,'api-worker'],stdout=subprocess.DEVNULL,stderr=subprocess.PIPE)
    def request(path,method='GET',body=b''):
        with socket.socket(socket.AF_UNIX) as client:
            client.settimeout(3);client.connect(str(root/'api.sock'))
            client.sendall(f'{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {len(body)}\r\n\r\n'.encode()+body)
            response=b''
            while data:=client.recv(8192):response+=data
        head,body=response.split(b'\r\n\r\n',1)
        return int(head.split()[1]),json.loads(body)
    try:
        deadline=time.monotonic()+5
        while not (root/'api.sock').exists():
            assert worker.poll() is None,worker.stderr.read().decode()
            assert time.monotonic()<deadline
            time.sleep(.02)
        deadline=time.monotonic()+3
        while True:
            code,sensors=request('/v1/sensors');assert code==200,sensors
            if len(sensors)==3 and all(s['ready'] for s in sensors):break
            assert time.monotonic()<deadline,sensors
            time.sleep(.02)
        indexed={s['id']:s for s in sensors}
        light=indexed['ambient-light:simulated']
        assert light['kind']=='light_intensity' and light['unit']=='raw' and light['value']==1234
        temperature=indexed['temperature:simulated']
        assert temperature['unit']=='celsius' and temperature['value']==.5
        adc=indexed['adc:simulated:0']
        assert adc['kind']=='adc' and adc['unit']=='raw' and adc['value']==100
        assert all(s['supported'] and s['simulated'] and s['age_ms']<3000 for s in sensors)
        assert request('/v1/sensors/ambient-light:simulated')[1]['value']==1234
        assert request('/v1/sensors/not-present')[0]==404
        assert request('/v1/sensors/%2E%2E%2Fsettings.json')[0]==404
        capability=request('/v1/capabilities')[1][0]
        assert capability['id']=='sensors.read.v1' and capability['ready'] and capability['simulated']
        address=request('/v1/health')[1]['http']
        req=urllib.request.Request(f'http://{address}/v1/sensors',headers={'Authorization':'Bearer '+key.read_text()})
        with urllib.request.urlopen(req,timeout=3) as reply:
            assert len(json.load(reply))==3
        changed={**config,'sensors':{'backend':'off'}}
        (root/'settings.json').write_text(json.dumps(changed))
        code,error=request('/v1/config/reload','POST',b'{}')
        assert code==400 and 'restart' in error['error']['message']
        assert request('/v1/sensors/temperature:simulated')[1]['ready']
        print('PASS simulated sensors, exact units, read scope, registry lookup and reload isolation')
    finally:
        if worker.poll() is None:
            worker.terminate()
            try:worker.wait(timeout=5)
            except subprocess.TimeoutExpired:worker.kill();worker.wait()
