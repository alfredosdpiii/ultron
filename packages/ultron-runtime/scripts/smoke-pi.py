"""No-model startup test of the installed Pi extension command registration."""
import json
import os
import selectors
import subprocess
import time

p = subprocess.Popen(['pi', '--mode', 'rpc', '--offline', '--no-session'], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=False)
s = selectors.DefaultSelector()
s.register(p.stdout, selectors.EVENT_READ, 'stdout')
s.register(p.stderr, selectors.EVENT_READ, 'stderr')
p.stdin.write(b'{"id":"commands","type":"get_commands"}\n')
p.stdin.flush()
buffers = {'stdout': b'', 'stderr': b''}
try:
    end = time.monotonic() + 25
    while time.monotonic() < end:
        for key, _ in s.select(0.2):
            chunk = os.read(key.fileobj.fileno(), 65536)
            if not chunk:
                s.unregister(key.fileobj)
                continue
            buffers[key.data] += chunk
            if key.data != 'stdout':
                continue
            while b'\n' in buffers['stdout']:
                line, buffers['stdout'] = buffers['stdout'].split(b'\n', 1)
                try:
                    event = json.loads(line)
                except ValueError:
                    continue
                if event.get('id') == 'commands':
                    assert event.get('success'), event
                    commands = event.get('data', {}).get('commands', [])
                    names = [c['name'] for c in commands]
                    for name in ['agents', 'memory', 'refine-proposals', 'experiments', 'background', 'rlm', 'rlm-children', 'jev']:
                        assert names.count(name) == 1, (name, names)
                    print('PASS: agents, memory, refine-proposals, experiments, background, rlm, rlm-children, jev each registered once; no model request.')
                    raise SystemExit(0)
    raise RuntimeError('Pi startup did not return command list: ' + buffers['stderr'].decode(errors='replace')[-3000:])
finally:
    p.terminate()
    try:
        p.wait(timeout=5)
    except subprocess.TimeoutExpired:
        p.kill()
        p.wait(timeout=5)
    s.close()
