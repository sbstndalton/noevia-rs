# Synthetic ACP engine. The descendant proves teardown covers the process group, not admission.
import json
import os
import signal
import subprocess
import sys
import time

if os.environ.get('IGNORE_TERM') == '1':
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
subprocess.Popen([sys.executable, '-c', "import os,time\np=os.environ['HEARTBEAT']\nwhile True:\n with open(p,'a') as f:f.write('x')\n time.sleep(.01)"])
while not os.path.exists(os.environ['HEARTBEAT']):
    time.sleep(.002)
with open(os.environ['OBSERVED_ENV'], 'w') as output:
    json.dump(dict(os.environ), output)
for line in sys.stdin:
    request = json.loads(line)
    method = request.get('method')
    if method == 'initialize':
        result = {'protocolVersion': 1, 'agentInfo': {'name': 'synthetic', 'version': '1'}}
    elif method == 'session/new':
        result = {'sessionId': 'spawn-session'}
    elif method == 'session/prompt':
        result = {'stopReason': 'end_turn'}
    elif method == 'session/cancel':
        with open(os.environ['CANCELLED'], 'w') as output:
            output.write('cancelled')
        continue
    else:
        raise RuntimeError('Unexpected launch frame or method')
    if os.environ.get('STALL_AT') == method:
        with open(os.environ['ENTERED'], 'w') as output:
            output.write(method)
        while True:
            time.sleep(.01)
    print(json.dumps({'jsonrpc': '2.0', 'id': request['id'], 'result': result}), flush=True)
