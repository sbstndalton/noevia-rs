"""Failure-only metadata trace of the exact, already built synthetic lifecycle test.

Never attaches to a preexisting PID, installs tools, changes host protections or
traces socket/file payloads. Its result cannot replace the required test result.
"""
import hashlib
import json
import os
from pathlib import Path
import resource
import stat
import subprocess
import sys

TEST = 'node_reader_distinguishes_complete_output_from_post_exit_reset'
LIMIT = 4 * 1024 * 1024


def select_binary(events, workspace):
    source = workspace / 'bins/code-sandbox-supervisor/tests/lifecycle.rs'
    found = set()
    failed = False
    if events.stat().st_size > 32 * 1024 * 1024:
        raise ValueError('compiler event stream exceeded bound')
    with events.open() as stream:
        for line in stream:
            if len(line) > 1024 * 1024:
                raise ValueError('compiler event line exceeded bound')
            failed |= f'---- {TEST} stdout ----' in line
            try:
                item = json.loads(line)
            except json.JSONDecodeError:
                continue  # libtest output shares Cargo's stdout.
            if not isinstance(item, dict):
                continue
            target = item.get('target', {})
            if (item.get('reason') != 'compiler-artifact'
                    or target.get('kind') != ['test']
                    or target.get('name') != 'lifecycle'
                    or target.get('src_path') != str(source)
                    or not item.get('profile', {}).get('test')):
                continue
            name = item.get('executable')
            if not isinstance(name, str):
                raise ValueError('missing lifecycle executable')
            path = Path(name)
            resolved = path.resolve(strict=True)
            info = path.lstat()
            if (resolved != path or not resolved.is_relative_to(workspace / 'target')
                    or not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid()
                    or not info.st_mode & stat.S_IXUSR):
                raise ValueError('lifecycle executable is not the owned build artifact')
            found.add(resolved)
    if not failed:
        raise ValueError('target test did not fail; no diagnostic rerun allowed')
    if len(found) != 1:
        raise ValueError('missing or ambiguous lifecycle build artifact')
    return found.pop()


def main():
    if sys.platform != 'linux' or os.environ.get('GITHUB_ACTIONS') != 'true' or os.geteuid() == 0:
        raise RuntimeError('non-root owned GitHub Linux diagnostic required')
    workspace = Path(os.environ['GITHUB_WORKSPACE']).resolve(strict=True)
    temporary = Path(os.environ['RUNNER_TEMP']).resolve(strict=True)
    output = temporary / 'noevia-supervisor-reset-trace'
    output.mkdir(mode=0o700)  # Refuse existing/symlinked output, never upload another directory.
    status = {'test': TEST, 'state': 'unavailable', 'attempted': False}
    try:
        binary = select_binary(temporary / 'noevia-workspace-test-events.jsonl', workspace)
        status['revision'] = subprocess.check_output(
            ['git', 'rev-parse', 'HEAD'], cwd=workspace, text=True, timeout=5).strip()
        status['executable'] = str(binary)
        with binary.open('rb') as stream:
            status['binarySha256'] = hashlib.file_digest(stream, 'sha256').hexdigest()
        tracer = Path('/usr/bin/strace')
        if not tracer.is_file() or not os.access(tracer, os.X_OK):
            status['reason'] = 'required existing strace is unavailable; no installation attempted'
            return
        help_result = subprocess.run([str(tracer), '--help'], capture_output=True, timeout=5, text=True)
        if help_result.returncode != 0 or '--kill-on-exit' not in help_result.stdout:
            status['reason'] = 'existing strace does not provide required owned-tracee exit cleanup'
            return
        status['attempted'] = True
        trace = output / 'socket-metadata.log'
        log = output / 'test.log'
        # A single trace and combined test log are each kernel-capped to 4 MiB.
        # File limits do not cap the test's 64 MiB socket/pipe stream.
        resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
        resource.setrlimit(resource.RLIMIT_FSIZE, (LIMIT, LIMIT))
        # Raw read/recv arguments are numeric only: never decode buffers,
        # iovecs or message structures. Correlate their FD/PID with the decoded
        # socket metadata. Epoll arrays contain event flags/IDs, not payloads.
        command = [str(tracer), '-f', '--kill-on-exit',
                   '-ttt', '-yy', '-e', 'trace=accept,accept4,connect,setsockopt,getsockopt,shutdown,close,dup,dup2,dup3,fcntl,read,readv,recvfrom,recvmsg,recvmmsg,epoll_wait,epoll_pwait',
                   '-e', 'raw=read,readv,recvfrom,recvmsg,recvmmsg',
                   '-o', str(trace), str(binary), '--exact', TEST, '--nocapture', '--test-threads=1']
        with log.open('wb') as stream:
            process = subprocess.Popen(command, stdout=stream, stderr=subprocess.STDOUT,
                                       start_new_session=True)
            status['timedOut'] = False
            try:
                code = process.wait(timeout=60)
            except subprocess.TimeoutExpired:
                status['timedOut'] = True
                # Kill/reap only this still-owned tracer PID. --kill-on-exit
                # makes the kernel kill its tracees, including their sessions.
                process.kill()
                code = process.wait(timeout=10)
        status['exitCode'] = code
        status['fileLimitReached'] = any(p.exists() and p.stat().st_size >= LIMIT for p in (trace, log))
        text = log.read_text(errors='replace')
        status['policyDenied'] = 'ptrace' in text.lower() and any(
            phrase in text.lower() for phrase in ('operation not permitted', 'permission denied'))
        status['testCompleted'] = TEST in text and 'test result:' in text and code in (0, 101)
        status['state'] = ('incomplete' if status['timedOut'] or status['fileLimitReached']
                           else 'unavailable' if status['policyDenied']
                           else 'recorded' if status['testCompleted'] and trace.exists()
                           else 'incomplete')
        if status['policyDenied']:
            status['reason'] = 'tracing was denied; no security change or alternate attachment attempted'
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
        status['reason'] = type(error).__name__ + ': ' + str(error)[:512]
    finally:
        (output / 'status.json').write_text(json.dumps(status, indent=2) + '\n')
        print(json.dumps(status))
        with Path(os.environ['GITHUB_OUTPUT']).open('a') as stream:
            stream.write('ready=true\n')


if __name__ == '__main__':
    main()
