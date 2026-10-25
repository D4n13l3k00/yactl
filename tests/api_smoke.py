#!/usr/bin/env python3
"""Exercise the real API worker without hardware writes."""
import json, pathlib, socket, subprocess, sys, tempfile, time, urllib.request, urllib.error
from concurrent.futures import ThreadPoolExecutor
binary = str(pathlib.Path(sys.argv[1]).resolve())
with tempfile.TemporaryDirectory(prefix='yactl-api-') as tmp:
    root = pathlib.Path(tmp)
    token='r'*32
    key=root/'read.token';key.write_text(token);key.chmod(0o600)
    config={'http': {'listen':'127.0.0.1:0'},'keys':[{'file':str(key),'scopes':['read']}]}
    (root/'settings.json').write_text(json.dumps(config))
    worker = subprocess.Popen([binary, '--root', tmp, 'api-worker'], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    def request(path, method='GET', body=b''):
        with socket.socket(socket.AF_UNIX) as s:
            s.settimeout(3); s.connect(str(root/'api.sock'))
            s.sendall(f'{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {len(body)}\r\n\r\n'.encode()+body)
            data=b''
            while chunk:=s.recv(65536): data+=chunk
        head, data=data.split(b'\r\n\r\n',1)
        return int(head.split()[1]), json.loads(data)
    try:
        end=time.monotonic()+8
        while not (root/'api.sock').exists():
            if worker.poll() is not None: raise AssertionError(worker.stderr.read().decode())
            assert time.monotonic()<end, 'API startup timeout'
            time.sleep(.02)
        assert (root/'api.sock').stat().st_mode & 0o777 == 0o600
        code, health=request('/v1/health'); assert code==200 and health['ready']
        assert not health['supervisor']['ready'] and health['providers']==[]
        address=health['http']
        def http(path, method='GET', token=None):
            headers={'Content-Type':'application/json'}
            if token:headers['Authorization']='Bearer '+token
            req=urllib.request.Request('http://'+address+path, data=b'{}' if method=='POST' else None,headers=headers,method=method)
            try:
                with urllib.request.urlopen(req,timeout=4) as r:return r.status,json.load(r)
            except urllib.error.HTTPError as e:return e.code,json.load(e)
        assert http('/v1/device')[0]==200
        assert http('/v1/services/request','POST')[0]==403
        assert http('/v1/services/request','POST',token)[0]==403
        assert http('/v1/lighting/stream','POST')[0]==403
        assert http('/v1/lighting/stream','POST',token)[0]==403
        assert http('/v1/clock/stream','POST')[0]==403
        assert http('/v1/clock/stream','POST',token)[0]==403
        assert http('/v1/device',token='wrong')[0]==401
        assert http('/v1/device',token=token)[0]==200
        code, device=request('/v1/device'); assert code==200 and len(device['instance_id'])==32
        identity=device['instance_id']
        configured={**config,'device':{'display_name':'Living room','model':'midi'}}
        (root/'settings.json').write_text(json.dumps(configured))
        assert request('/v1/config/reload','POST',b'{}')[0]==200
        updated=request('/v1/device')[1]
        assert updated['display_name']=='Living room' and updated['model']=='midi'
        assert updated['codename']=='cucumber' and updated['model_source']=='override'
        assert updated['instance_id']==identity and 'evidence' in updated and 'firmware' in updated
        invalid={**configured,'device':{'display_name':'','model':'midi'}}
        (root/'settings.json').write_text(json.dumps(invalid))
        assert request('/v1/config/reload','POST',b'{}')[0]==400
        assert request('/v1/device')[1]['display_name']=='Living room'
        assert request('/v1/capabilities')==(200, [])
        (root/'settings.json').write_text('{broken')
        assert request('/v1/config/reload','POST',b'{}')[0]==400
        assert request('/v1/health')[1]['ready']
        # Invalid key permissions also leave the old in-memory policy active.
        (root/'settings.json').write_text(json.dumps(config));key.chmod(0o644)
        assert request('/v1/config/reload','POST',b'{}')[0]==400
        assert http('/v1/device',token=token)[0]==200
        key.chmod(0o600)
        assert request('/v1/config/reload','POST',b'{}')[0]==200
        with socket.socket() as busy:
            busy.bind(('127.0.0.1',0));busy.listen()
            changed={**config,'http':{'listen':f'127.0.0.1:{busy.getsockname()[1]}'}}
            (root/'settings.json').write_text(json.dumps(changed))
            assert request('/v1/config/reload','POST',b'{}')[0]==400
            assert http('/v1/health')[0]==200
        assert request('/missing')[0]==404
        code,invalid=request('/v1/services/request','POST',b'{}')
        assert code==422 and invalid['error']['code']=='invalid_request'
        code,invalid=request('/v1/config/reload')
        assert code==405 and invalid['error']['code']=='method_not_allowed'
        # A stalled supervisor must not consume unbounded blocking workers.
        with socket.socket(socket.AF_UNIX) as stalled:
            stalled.bind(str(root/'control.sock')); stalled.listen(32)
            start=time.monotonic()
            assert not request('/v1/health')[1]['supervisor']['ready']
            assert time.monotonic()-start<1, 'health probe is unbounded'
            with ThreadPoolExecutor(max_workers=12) as pool:
                results=list(pool.map(lambda _: request('/v1/services/request','POST',b'{"action":"ping"}')[0], range(12)))
            assert 503 in results, 'service forwarding lacks blocking admission control'
        (root/'control.sock').unlink()
        assert request('/v1/health')[0]==200
        (root/'settings.json').write_text(json.dumps(configured))
        worker.terminate(); worker.wait(timeout=5)
        worker = subprocess.Popen([binary, '--root', tmp, 'api-worker'], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        end=time.monotonic()+5
        while not (root/'api.sock').exists():
            assert worker.poll() is None, worker.stderr.read().decode()
            assert time.monotonic()<end, 'API restart timeout'
            time.sleep(.02)
        restored=request('/v1/device')[1]
        assert restored['instance_id']==identity and restored['display_name']=='Living room'
        print('PASS private API, identity, bounded routes and invalid reload isolation')
    finally:
        worker.terminate()
        try: worker.wait(timeout=5)
        except subprocess.TimeoutExpired: worker.kill(); worker.wait()
