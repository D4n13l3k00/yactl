#!/usr/bin/env python3
"""Persistent and temporary volume changes use a simulated mixer."""
import json
import pathlib
import socket
import subprocess
import sys
import tempfile
import time
import urllib.request
import urllib.error

binary=str(pathlib.Path(sys.argv[1]).resolve())
with tempfile.TemporaryDirectory(prefix='yactl-volume-') as tmp:
    root=pathlib.Path(tmp)
    reader=root/'reader.token';reader.write_text('r'*32);reader.chmod(0o600)
    controller=root/'controller.token';controller.write_text('h'*32);controller.chmod(0o600)
    config={'http':{'listen':'127.0.0.1:0'},'volume':{'backend':'simulate'},'keys':[{'file':str(reader),'scopes':['read']},{'file':str(controller),'scopes':['hardware']}]}
    (root/'settings.json').write_text(json.dumps(config))
    worker=subprocess.Popen([binary,'--root',tmp,'api-worker'],stdout=subprocess.DEVNULL,stderr=subprocess.PIPE)
    def request(path,method='GET',data=None):
        body=b'' if data is None else json.dumps(data).encode()
        with socket.socket(socket.AF_UNIX) as client:
            client.settimeout(3);client.connect(str(root/'api.sock'))
            client.sendall(f'{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {len(body)}\r\n\r\n'.encode()+body)
            response=b''
            while chunk:=client.recv(8192):response+=chunk
        header,body=response.split(b'\r\n\r\n',1)
        return int(header.split()[1]),json.loads(body)
    def wait(predicate):
        until=time.monotonic()+4
        while True:
            status=request('/v1/volume')[1]
            if predicate(status):return status
            assert time.monotonic()<until,status
            time.sleep(.02)
    try:
        until=time.monotonic()+5
        while not (root/'api.sock').exists():
            assert worker.poll() is None,worker.stderr.read().decode()
            assert time.monotonic()<until
            time.sleep(.02)
        original=wait(lambda s:s['ready'])['raw']
        cli=subprocess.run([binary,'--root',tmp,'--json','volume','status'],capture_output=True,text=True,timeout=3)
        assert cli.returncode==0,cli.stderr
        assert json.loads(cli.stdout)['raw']==original
        code,result=request('/v1/volume','POST',{'percent':50})
        assert code==200,result
        assert result['status']['raw']==[128,128] and not result['status']['temporary']
        code,result=request('/v1/volume','POST',{'percent':10,'lease_ms':250})
        assert code==200 and result['status']['temporary'],result
        assert wait(lambda s:not s['temporary'])['raw']==[128,128]
        request('/v1/volume','POST',{'percent':10,'lease_ms':250})
        request('/v1/volume','POST',{'percent':70})
        time.sleep(.35)
        assert request('/v1/volume')[1]['raw']==[179,179], 'old temporary restore overwrote newer permanent value'
        before=request('/v1/volume')[1]['raw']
        for invalid in [{'percent':101},{'percent':-1},{'percent':float('nan')},{'percent':20,'lease_ms':0},{'percent':20,'lease_ms':5001},{'percent':20,'control':'Other'}]:
            assert request('/v1/volume','POST',invalid)[0] in (400,422),invalid
            assert request('/v1/volume')[1]['raw']==before
        address=request('/v1/health')[1]['http']
        def http(path,method,body,token=None):
            headers={'Content-Type':'application/json'}
            if token:headers['Authorization']='Bearer '+token
            req=urllib.request.Request('http://'+address+path,method=method,data=json.dumps(body).encode() if body is not None else None,headers=headers)
            try:
                with urllib.request.urlopen(req,timeout=3) as reply:return reply.status,json.load(reply)
            except urllib.error.HTTPError as reply:return reply.code,json.load(reply)
        assert http('/v1/volume','GET',None,reader.read_text())[0]==200
        assert http('/v1/volume','POST',{'percent':30},reader.read_text())[0]==403
        assert http('/v1/volume','POST',{'percent':30})[0]==403
        assert http('/v1/volume','POST',{'percent':30},controller.read_text())[0]==200
        assert http('/v1/services/request','POST',{'action':'ping'},controller.read_text())[0]==403
        assert request('/v1/capabilities')[1][0]['id']=='audio.volume.v1'
        cli=subprocess.run([binary,'--root',tmp,'--json','volume','set','40','--lease-ms','100'],capture_output=True,text=True,timeout=3)
        assert cli.returncode==0,cli.stderr
        assert json.loads(cli.stdout)['status']['temporary']
        print('PASS simulated volume, rounding, leases, superseding writes, validation and authorization')
    finally:
        if worker.poll() is None:
            worker.terminate()
            try:worker.wait(timeout=5)
            except subprocess.TimeoutExpired:worker.kill();worker.wait()
