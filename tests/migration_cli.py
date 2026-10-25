#!/usr/bin/env python3
"""Check real CLI names/defaults without starting services or touching hardware."""
import json
import pathlib
import subprocess
import sys
import tempfile

binary = str(pathlib.Path(sys.argv[1]).resolve())
with tempfile.TemporaryDirectory(prefix='yactl-cli-') as tmp:
    def run(*args):
        return subprocess.run([binary, '--root', tmp, *args], capture_output=True, text=True, timeout=10)
    help_text = run('--help')
    assert help_text.returncode == 0 and 'yactl' in help_text.stdout
    grouped = run('services', '--help')
    assert grouped.returncode == 0 and 'start' in grouped.stdout
    old_style = run('status')
    new_style = run('services', 'status')
    assert old_style.returncode != 0 and new_style.returncode != 0
    assert 'control.sock' in old_style.stderr and 'control.sock' in new_style.stderr
    volume_help=run('volume','--help')
    assert volume_help.returncode==0 and 'set' in volume_help.stdout
    for args in [('volume','set','101'),('volume','set','20','--lease-ms','99')]:
        invalid=run(*args)
        assert invalid.returncode==2 and 'api.sock' not in invalid.stderr
print('PASS yactl CLI and compatibility command forms')
