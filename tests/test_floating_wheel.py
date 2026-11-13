"""Opt-in Windows+Rex UI regression; run from WSL after a release build.

python3 tests/test_floating_wheel.py
Uses only a private server, generated receiver processes and a temporary profile.
Never sends keys to, or stops, the installed/user flex process.
Requires rex, PowerShell.exe, websockets and target/release/flex.exe.
"""
import asyncio
import base64
import json
import os
from pathlib import Path
import re
import socket
import subprocess
import tempfile
import time

import websockets

ROOT = Path(__file__).resolve().parents[1]
EXE = ROOT / 'target/release/flex.exe'


def windows(path):
    return subprocess.check_output(['wslpath', '-w', str(path)], text=True).strip()


async def check(base, folder):
    async with websockets.connect(base.replace('http://', 'ws://') + '/api/control', subprotocols=['rex.control.v1']) as ws:
        await ws.send(json.dumps({'type': 'hello', 'client': {'name': 'wheel-test'}, 'api_revision': 13, 'api_min_revision': 8}))
        await ws.recv()
        serial = 0

        async def rpc(method, payload):
            nonlocal serial
            serial += 1
            ident = str(serial)
            await ws.send(json.dumps({'type': 'request', 'id': ident, 'method': method, 'payload': payload}))
            while True:
                result = json.loads(await ws.recv())
                if result.get('id') == ident and result.get('type') == 'response':
                    assert not result.get('error'), result.get('error')
                    return result.get('payload', {})

        receiver = folder / 'receiver.py'
        receiver.write_text('import os,sys,tty\ntty.setraw(0)\nprint("\\033[?1000h\\033[?1006hREADY",end="",flush=True)\nwith open(sys.argv[1],"ab",buffering=0) as log:\n while True:\n  data=os.read(0,4096)\n  if not data: break\n  log.write(data)\n')

        def layout(name):
            return {'block': {'flavor': 'com.superlogical.terminal.shell', 'options': {'shell': 'none', 'command': ['/usr/bin/python3', str(receiver), str(folder / (name + '.input'))]}}}

        await rpc('session.attach_all', {'event_scope': 'all'})
        created = await rpc('session.create', {'label': 'isolated wheel test', 'initial_windows': [{'layout': layout('bottom')}]})
        sid = created['session_id']
        wid = created['initial_windows'][0]['window_id']
        await rpc('session.attach', {'session_id': sid})
        await rpc('session.new_layer', {'session_id': sid, 'window_id': wid, 'bounds': [0.2, 0.2, 0.6, 0.6], 'layout': layout('float'), 'focus': True})
        pid = None
        try:
            appdata = folder / 'appdata/flex'
            appdata.mkdir(parents=True)
            (appdata / 'shrew.json').write_text(json.dumps({'servers': [], 'settings': {'flex.sidebar': False, 'flex.files_open': False, 'flex.acrylic': False}}))
            script = f"$env:APPDATA='{windows(folder / 'appdata')}'; $p=Start-Process '{windows(EXE)}' -ArgumentList '{base}' -PassThru; $p.Id | Set-Content '{windows(folder / 'pid.txt')}'"
            encoded = base64.b64encode(script.encode('utf-16le')).decode()
            with open(folder / 'launch.log', 'w') as log:
                subprocess.Popen(['powershell.exe', '-NoProfile', '-EncodedCommand', encoded], stdout=log, stderr=log)
            until = time.monotonic() + 15
            while not (folder / 'pid.txt').exists():
                assert time.monotonic() < until, 'test application did not start'
                await asyncio.sleep(.1)
            pid = int((folder / 'pid.txt').read_text(encoding='utf-8-sig'))
            await asyncio.sleep(4)

            def counts():
                def count(name):
                    path = folder / (name + '.input')
                    data = path.read_bytes() if path.exists() else b''
                    return len(re.findall(rb'\x1b\[<6[45];\d+;\d+M', data))
                return count('float'), count('bottom')

            for area in ('content', 'title', 'outside'):
                before = counts()
                driver = subprocess.run(['powershell.exe', '-NoProfile', '-File', windows(ROOT / 'tests/floating_wheel.ps1'), '-TestPid', str(pid), '-ExpectedExe', windows(EXE), '-Area', area], text=True, capture_output=True, timeout=10)
                assert driver.returncode == 0, driver.stderr
                await asyncio.sleep(.3)
                after = counts()
                delta = tuple(a - b for a, b in zip(after, before))
                print(f'{area}: float wheel events={delta[0]}, bottom wheel events={delta[1]}', flush=True)
                if area == 'content':
                    assert delta[0] > 0, 'wheel missed the test float'
                    assert delta[1] == 0, 'wheel leaked through float to the terminal underneath'
                elif area == 'title':
                    assert delta == (0, 0), 'wheel on float title leaked into a terminal'
                else:
                    assert delta[0] == 0 and delta[1] > 0, 'uncovered terminal must still receive its wheel'
        finally:
            if pid:
                subprocess.run(['powershell.exe', '-NoProfile', '-Command', f"$p=Get-Process -Id {pid} -ErrorAction SilentlyContinue; if($p -and $p.Path -eq '{windows(EXE)}'){{Stop-Process -Id {pid}}}"], capture_output=True, timeout=10)
            await rpc('session.destroy', {'session_id': sid})


def main():
    assert EXE.exists(), 'Build the release executable first'
    with tempfile.TemporaryDirectory(prefix='floating-wheel-', dir=ROOT / 'dist') as tmp:
        folder = Path(tmp)
        (folder / 'init.lua').write_text('')
        with socket.socket() as listener:
            listener.bind(('127.0.0.1', 0))
            port = listener.getsockname()[1]
        base = f'http://127.0.0.1:{port}'
        with open(folder / 'server.log', 'w') as log:
            server = subprocess.Popen(['rex', 'server', 'run', '--listen', base, '--allow-anonymous', '--restore=false', '--state-dir', str(folder / 'state'), '--config', str(folder / 'init.lua')], stdout=log, stderr=log)
            try:
                time.sleep(1)
                asyncio.run(check(base, folder))
            finally:
                server.terminate()
                server.wait(timeout=5)


if __name__ == '__main__':
    main()
