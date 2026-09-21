#!/usr/bin/env python3
import errno
import faulthandler
import fcntl
import json
import os
from pathlib import Path
import pty
import resource
import select
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time
import unittest

BINARY = str(Path(__file__).resolve().parents[1] / 'target/debug/termlog')

class Terminal:
    def __init__(self, root, args, limit=None):
        self.root = Path(root)
        self.master, self.slave = pty.openpty()
        os.set_blocking(self.master, False)
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack('HHHH', 24, 100, 0, 0))
        self.original = termios.tcgetattr(self.master)
        self.data = bytearray()
        self.closed = False
        env = dict(os.environ, XDG_STATE_HOME=str(root), XDG_CONFIG_HOME=str(self.root/'config'), TERM='xterm-256color', SHELL='/bin/bash', PS1='READY> ')
        for key in ('TERMLOG_ACTIVE','TERMLOG_SESSION_ID'):
            env.pop(key, None)
        def setup():
            os.setsid()
            fcntl.ioctl(0, termios.TIOCSCTTY, 0)
            if limit:
                signal.signal(signal.SIGXFSZ, signal.SIG_IGN)
                resource.setrlimit(resource.RLIMIT_FSIZE, (limit,limit))
        self.process = subprocess.Popen([BINARY,*args], stdin=self.slave, stdout=self.slave, stderr=self.slave, env=env, preexec_fn=setup)
    def send(self, data): os.write(self.master, data)
    def read(self, timeout=.05):
        if select.select([self.master],[],[],timeout)[0]:
            try:
                data=os.read(self.master,65536)
                self.data.extend(data)
                return bool(data)
            except OSError as e:
                if e.errno not in (errno.EIO,errno.EAGAIN): raise
        return False
    def until(self, text, timeout=10):
        end=time.monotonic()+timeout
        while text not in self.data and time.monotonic()<end:
            self.read()
            if self.process.poll() is not None: break
        if text not in self.data: raise AssertionError(f'missing {text!r}: {self.data[-1000:]!r}')
    def finish(self, timeout=40):
        end=time.monotonic()+timeout
        while self.process.poll() is None and time.monotonic()<end: self.read()
        if self.process.poll() is None: raise AssertionError('recorder deadlocked')
        # macOS keeps a hung-up PTY readable; stop draining at EOF.
        while self.read(0): pass
        self.restored = termios.tcgetattr(self.master) == self.original
        self.close()
        return self.process.returncode
    def close(self):
        if not self.closed:
            if self.process.poll() is None: self.process.kill();self.process.wait()
            os.close(self.master);os.close(self.slave);self.closed=True
    def path(self):
        paths=list(self.root.glob('termlog/sessions/*/*/*/*'))
        assert len(paths)==1,paths
        return paths[0]
    def metadata(self): return json.loads((self.path()/'metadata.json').read_text())
    def events(self): return [json.loads(line) for line in (self.path()/'events.cast').read_text().splitlines()[1:]]

class RecorderTests(unittest.TestCase):
    def setUp(self):
        faulthandler.dump_traceback_later(120, exit=True)
        self.addCleanup(faulthandler.cancel_dump_traceback_later)
        self.tmp=tempfile.TemporaryDirectory();self.root=Path(self.tmp.name)
        self.addCleanup(self.tmp.cleanup)
    def terminal(self,args,limit=None):
        t=Terminal(self.root,args,limit);self.addCleanup(t.close);return t
    def run_child(self,script,flags=(),limit=None):
        return self.terminal(['run',*flags,'--',sys.executable,'-c',script],limit)
    def config(self,text):
        path=self.root/'config/termlog';path.mkdir(parents=True,exist_ok=True)
        (path/'config.toml').write_text(text)
    def cli(self,*args):
        env=dict(os.environ,XDG_STATE_HOME=str(self.root),XDG_CONFIG_HOME=str(self.root/'config'))
        return subprocess.run([BINARY,*args],env=env,capture_output=True,timeout=10)
    def input_session(self,flags=(),config=''):
        self.config(config)
        script='''import termios
input("Visible: ")
a=termios.tcgetattr(0);a[3]&=~termios.ECHO;termios.tcsetattr(0,termios.TCSANOW,a)
input("Hidden: ")
print("done")'''
        t=self.run_child(script,flags);t.until(b'Visible: ');t.send(b'visible-command\n')
        t.until(b'Hidden: ');t.send(b'secret-password\n');self.assertEqual(t.finish(),0)
        self.assertTrue(t.restored)
        return t
    def test_safe_hidden_input_and_echo(self):
        t=self.input_session()
        self.assertNotIn('secret-password',(t.path()/'events.cast').read_text())
        self.assertFalse(any(e[1]=='i' for e in t.events()))
        self.assertIn('visible-command',''.join(e[2] for e in t.events() if e[1]=='o'))
    def test_full_hidden_input(self):
        t=self.input_session(['--capture-input'])
        self.assertIn('secret-password',''.join(e[2] for e in t.events() if e[1]=='i'))
        self.assertNotIn('secret-password',(t.path()/'transcript.log').read_text())
    def test_cli_safe_overrides_config(self):
        t=self.input_session(['--no-capture-input'],'capture_input=true\n')
        self.assertFalse(t.metadata()['capture_input'])
        self.assertNotIn('secret-password',(t.path()/'events.cast').read_text())
    def test_conflicting_flags(self):
        for command in ('shell','run'):
            args=[command,'--capture-input','--no-capture-input']
            if command=='run':args+=['--','/bin/true']
            self.assertNotEqual(self.cli(*args).returncode,0)
    def test_exit_status(self):
        t=self.run_child('raise SystemExit(42)');self.assertEqual(t.finish(),42)
        self.assertEqual(t.events()[-1][1:],['x','42'])
        self.assertEqual(t.metadata()['exit_code'],42)
        self.assertTrue(t.metadata()['recording_complete']);self.assertTrue(t.metadata()['transcript_complete'])
        self.assertEqual(t.path().stat().st_mode&0o777,0o700)
        for file in t.path().iterdir(): self.assertEqual(file.stat().st_mode&0o777,0o600)
    def test_resize(self):
        t=self.run_child('import os; input("resize-ready"); print("SIZE",os.get_terminal_size().columns,os.get_terminal_size().lines)')
        t.until(b'resize-ready');fcntl.ioctl(t.master,termios.TIOCSWINSZ,struct.pack('HHHH',40,120,0,0))
        os.kill(t.process.pid,signal.SIGWINCH);time.sleep(.15);t.send(b'\n')
        self.assertEqual(t.finish(),0);self.assertIn(b'SIZE 120 40',t.data)
        self.assertTrue(any(e[1:]==['r','120x40'] for e in t.events()))
    def test_nested_shell_and_recorder(self):
        self.config('capture_input=true\n[shell]\ncommand="/bin/bash"\nargs=["--noprofile","--norc","-i"]\n')
        t=self.terminal(['shell','--no-capture-input']);t.until(b'READY> ')
        t.send(f'{BINARY} shell --no-capture-input\n'.encode())
        time.sleep(.15);t.send(b'printf "nested-ok\\n"\nexit\nexit\n')
        self.assertEqual(t.finish(),0);self.assertIn(b'nested-ok',t.data)
        self.assertFalse(t.metadata()['capture_input']);self.assertFalse(any(e[1]=='i' for e in t.events()))
    def test_abnormal_termination(self):
        self.config('flush_interval_ms=0\n')
        t=self.run_child('import time; print("crash-ready",flush=True); time.sleep(30)')
        t.until(b'crash-ready');time.sleep(.1);t.process.kill();t.finish()
        self.assertFalse(t.metadata()['recording_complete']);self.assertFalse(t.metadata()['transcript_complete'])
        self.assertFalse(any(e[1]=='x' for e in t.events()))
    def test_split_utf8(self):
        self.config('flush_interval_ms=0\n')
        t=self.run_child('import os,time\nfor b in "日本語".encode():\n os.write(1,bytes([b]));time.sleep(.05)\nos.write(1,b"\\n")')
        self.assertEqual(t.finish(),0)
        self.assertEqual(''.join(e[2] for e in t.events() if e[1]=='o'),'日本語\r\n')
        self.assertTrue(t.metadata()['recording_complete']);self.assertEqual(t.metadata()['utf8_replacements'],0)
        original=(t.path()/'transcript.log').read_bytes()
        self.assertEqual(self.cli('rebuild',t.path().name).returncode,0)
        self.assertEqual((t.path()/'transcript.log').read_bytes(),original)
    def test_invalid_utf8_marks_only_the_original_loss(self):
        t=self.run_child('import os;os.write(1,bytes([255]))')
        self.assertEqual(t.finish(),0)
        self.assertIn(255,t.data);self.assertFalse(t.metadata()['recording_complete'])
        self.assertTrue(t.metadata()['transcript_complete']);self.assertEqual(t.metadata()['utf8_replacements'],1)
        self.assertIsNotNone(t.metadata()['recording_error'])
    def test_large_output(self):
        # 32 MiB of output: validate the terminal stream and every cast byte.
        chunk=b'0123456789abcdef'*64
        t=self.run_child('import os\nchunk=b"0123456789abcdef"*64\nfor _ in range(32768): os.write(1,chunk)')
        self.assertEqual(t.finish(timeout=90),0)
        expected=chunk*32768
        self.assertEqual(t.data,expected)
        self.assertEqual(''.join(e[2] for e in t.events() if e[1]=='o').encode(),expected)
        self.assertTrue(t.metadata()['recording_complete'])
    def test_transcript_write_failure_and_rebuild(self):
        # Timestamp prefixes make transcript exceed the limit while cast stays small.
        t=self.run_child('import os;os.write(1,b"x\\n"*3000)',limit=32768)
        self.assertEqual(t.finish(),0)
        metadata=t.metadata();self.assertTrue(metadata['recording_complete']);self.assertFalse(metadata['transcript_complete'])
        self.assertTrue(metadata['transcript_error']);self.assertEqual(t.events()[-1][1],'x')
        rebuilt=self.cli('rebuild',t.path().name);self.assertEqual(rebuilt.returncode,0,rebuilt.stderr)
        self.assertTrue(t.metadata()['transcript_complete']);self.assertIsNone(t.metadata()['transcript_error'])
        self.assertEqual(len((t.path()/'transcript.log').read_text().splitlines()),3002)
    def test_cast_failure_keeps_child_alive(self):
        t=self.run_child('import os;os.write(1,b"x"*150000);print("STILL_ALIVE")',limit=32768)
        self.assertEqual(t.finish(),0);self.assertIn(b'STILL_ALIVE',t.data)
        self.assertFalse(t.metadata()['recording_complete']);self.assertTrue(t.metadata()['recording_error'])
    def test_existing_storage_permissions_unchanged(self):
        storage=self.root/'existing';storage.mkdir();storage.chmod(0o755)
        self.config('[storage]\npath='+json.dumps(str(storage))+'\n')
        t=self.run_child('print("SHOULD_NOT_RUN")');self.assertNotEqual(t.finish(),0)
        self.assertEqual(storage.stat().st_mode&0o777,0o755);self.assertNotIn(b'SHOULD_NOT_RUN',t.data)
        self.assertIn(b'accessible by other users',t.data)
        storage.chmod(0o700)
        private=self.run_child('print("private-ok")');self.assertEqual(private.finish(),0)
        self.assertEqual(storage.stat().st_mode&0o777,0o700)
    def test_symlink_storage_rejected_without_chmod(self):
        target=self.root/'target';target.mkdir();target.chmod(0o755)
        link=self.root/'link';link.symlink_to(target,target_is_directory=True)
        self.config('[storage]\npath='+json.dumps(str(link))+'\n')
        t=self.run_child('print("SHOULD_NOT_RUN")');self.assertNotEqual(t.finish(),0)
        self.assertEqual(target.stat().st_mode&0o777,0o755);self.assertNotIn(b'SHOULD_NOT_RUN',t.data)
    def test_list_uses_started_at(self):
        t=self.run_child('print("ok")');self.assertEqual(t.finish(),0)
        template=t.metadata();t.path().joinpath('metadata.json').unlink()
        parent=next((self.root/'termlog/sessions').glob('*/*/*'))
        ids=['ffffffff-ffff-4fff-8fff-ffffffffffff','00000000-0000-4000-8000-000000000000','aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa']
        for id,time_string in zip(ids,['2026-09-20T01:00:00Z','2026-09-20T23:00:00Z','2026-09-20T22:00:00Z']):
            path=parent/id;path.mkdir();metadata=dict(template,session_id=id,started_at=time_string)
            (path/'metadata.json').write_text(json.dumps(metadata))
        result=self.cli('list');self.assertEqual(result.returncode,0)
        self.assertEqual([line.split()[2] for line in result.stdout.decode().splitlines()],[ids[1],ids[2],ids[0]])
    def test_replay_filters_side_effects_and_ignores_resize(self):
        t=self.run_child('print("ok")');self.assertEqual(t.finish(),0);path=t.path()
        header=json.loads((path/'events.cast').read_text().splitlines()[0])
        events=[[0,'o','a\x1b]52;c;secret\x07\x1b]0;title\x07\x1bPpayload\x1b\\\x1b[8;55;180t\x1b[31mred\x1b[0m'],[0,'r','180x55'],[0,'x','0']]
        (path/'events.cast').write_text('\n'.join(json.dumps(e) for e in [header,*events])+'\n')
        replay=self.terminal(['replay',path.name]);self.assertEqual(replay.finish(),0)
        self.assertTrue(replay.restored);self.assertIn(b'\x1b[31mred',replay.data)
        for denied in [b']52',b'title',b'payload',b'\x1b[8;'] :self.assertNotIn(denied,replay.data)

    def test_incomplete_transcript_visibility(self):
        t=self.run_child('print("needle")');self.assertEqual(t.finish(),0);path=t.path()
        for pattern,code in [('needle',0),('absent',1)]:
            self.assertEqual(self.cli('search',pattern).returncode,code)
        metadata=t.metadata();metadata['transcript_complete']=False
        (path/'metadata.json').write_text(json.dumps(metadata))
        for pattern in ('needle','absent'):
            result=self.cli('search',pattern)
            self.assertEqual(result.returncode,2,result.stderr)
            self.assertIn(path.name.encode(),result.stderr)
            self.assertIn(b'Run: termlog rebuild',result.stderr)
            if pattern=='needle':self.assertIn(b'needle',result.stdout)
        shown=self.cli('show',path.name)
        self.assertEqual(shown.returncode,0);self.assertIn(b'needle',shown.stdout)
        self.assertIn(b'incomplete',shown.stderr);self.assertIn(path.name.encode(),shown.stderr)
        listed=self.cli('list')
        self.assertIn(b'recording=complete transcript=incomplete',listed.stdout)
        (path/'transcript.log').unlink()
        self.assertEqual(self.cli('search','needle').returncode,2)
        self.assertNotEqual(self.cli('show',path.name).returncode,0)
        metadata['transcript_complete']=True
        (path/'metadata.json').write_text(json.dumps(metadata))
        self.assertEqual(self.cli('search','absent').returncode,2)
        (path/'metadata.json').write_text('invalid json')
        result=self.cli('search','absent')
        self.assertEqual(result.returncode,2);self.assertIn(b'metadata.json',result.stderr)

    def test_search_order_and_timestamp_ties(self):
        t=self.run_child('print("needle")');self.assertEqual(t.finish(),0);path=t.path()
        template=t.metadata();template['started_at']='2026-09-20T00:00:00Z'
        (path/'metadata.json').write_text(json.dumps(template))
        ids=['ffffffff-ffff-4fff-8fff-ffffffffffff','00000000-0000-4000-8000-000000000000','aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa']
        for id,date in zip(ids,['2026-09-20T01:00:00Z','2026-09-20T23:00:00Z','2026-09-20T23:00:00Z']):
            session=path.parent/id;session.mkdir()
            (session/'metadata.json').write_text(json.dumps(dict(template,session_id=id,started_at=date)))
            (session/'transcript.log').write_text('2026-09-20 UTC+00:00\n\n00:00:00  needle\n')
        result=self.cli('search','--plain','needle');self.assertEqual(result.returncode,0,result.stderr)
        self.assertEqual([line.split(':')[0] for line in result.stdout.decode().splitlines()],[ids[1],ids[2],ids[0],path.name])
        result=self.cli('list');self.assertEqual(result.returncode,0)
        self.assertEqual([line.split()[2] for line in result.stdout.decode().splitlines()],[ids[1],ids[2],ids[0],path.name])

    def test_replay_restores_display_on_exit_and_interrupt(self):
        t=self.run_child('print("ok")');self.assertEqual(t.finish(),0);path=t.path()
        header=json.loads((path/'events.cast').read_text().splitlines()[0])
        reset=b'\x1b[0m\x1b[?6l\x1b[?7h\x1b[r\x1b[?25h\x1b[?1049l'
        for stop in ('normal','q','ctrl-c','sigint','sigterm','parser-error'):
            with self.subTest(stop=stop):
                events=[[0,'o','\x1b[?6h\x1b[?7l\x1b[5;10rREADY']]
                if stop=='parser-error':events.append([0,'o'])
                else:events.append([0 if stop=='normal' else 30,'x','0'])
                (path/'events.cast').write_text('\n'.join(json.dumps(e) for e in [header,*events])+'\n')
                replay=self.terminal(['replay',path.name]);replay.until(b'READY')
                if stop=='q':replay.send(b'q')
                elif stop=='ctrl-c':replay.send(b'\x03')
                elif stop=='sigint':replay.process.send_signal(signal.SIGINT)
                elif stop=='sigterm':replay.process.terminate()
                code=replay.finish()
                self.assertEqual(code,1 if stop=='parser-error' else 0,replay.data)
                self.assertTrue(replay.restored);self.assertIn(reset,replay.data)
                self.assertIn(b'\x1b[?6h\x1b[?7l\x1b[5;10r',replay.data)

    def test_readable_log_times_on_every_line_and_search_context(self):
        t=self.run_child('import os;os.write(1,"one\\ntwo\\n\\n  日本語\\nlast\\n".encode())')
        self.assertEqual(t.finish(),0);path=t.path()
        original=(path/'transcript.log').read_bytes()
        rows=original.decode().splitlines()
        self.assertRegex(rows[0],r'^\d{4}-\d{2}-\d{2} UTC[+-]\d{2}:\d{2}$')
        self.assertEqual(rows[1],'')
        self.assertEqual([row[10:] for row in rows[2:]],['one','two','','  日本語','last'])
        for row in rows[2:]:self.assertRegex(row,r'^\d{2}:\d{2}:\d{2}  ')
        self.assertEqual(t.metadata()['transcript_version'],2)
        shown=self.cli('show',path.name);self.assertEqual(shown.returncode,0,shown.stderr)
        self.assertIn(original,shown.stdout)
        self.assertIn(b'recording=complete transcript=complete',shown.stdout)
        result=self.cli('search','-C','1','two|日本語');self.assertEqual(result.returncode,0,result.stderr)
        self.assertEqual(result.stdout.count(path.name.encode()),1)
        self.assertIn(original,result.stdout)
        plain=self.cli('search','--plain','-F','日本語');self.assertEqual(plain.returncode,0,plain.stderr)
        self.assertTrue(plain.stdout.startswith((path.name+':6:').encode()))
        self.assertEqual(self.cli('rebuild',path.name).returncode,0)
        self.assertEqual(original,(path/'transcript.log').read_bytes())

    def test_legacy_rebuild_show_and_search(self):
        self.config('[transcript]\ntimestamps="rfc3339"\n')
        t=self.run_child('print("ok")');self.assertEqual(t.finish(),0);path=t.path()
        header=json.loads((path/'events.cast').read_text().splitlines()[0])
        self.assertEqual(t.metadata()['transcript_version'],1)
        self.assertEqual(header['termlog']['transcript_version'],1)
        header['termlog']['started_at']='2026-09-20T23:59:59+09:00'
        events=[[0,'o','before\r\n'],[2,'o','after\r\n'],[0,'x','0']]
        (path/'events.cast').write_text('\n'.join(json.dumps(e) for e in [header,*events])+'\n')
        metadata=t.metadata();metadata['transcript_version']=1
        (path/'metadata.json').write_text(json.dumps(metadata))
        rebuilt=self.cli('rebuild',path.name);self.assertEqual(rebuilt.returncode,0,rebuilt.stderr)
        original=(path/'transcript.log').read_bytes()
        self.assertEqual(original,b'2026-09-20T23:59:59.000000+09:00\tbefore\n2026-09-21T00:00:01.000000+09:00\tafter\n')
        shown=self.cli('show',path.name);self.assertEqual(shown.returncode,0,shown.stderr)
        self.assertIn(b'2026-09-20 UTC+09:00\n\n23:59:59  before',shown.stdout)
        self.assertIn(b'2026-09-21 UTC+09:00\n\n00:00:01  after',shown.stdout)
        result=self.cli('search','--plain','after');self.assertEqual(result.returncode,0,result.stderr)
        self.assertEqual(result.stdout,(path.name+':2:').encode()+original.splitlines(keepends=True)[1])
        self.assertEqual(original,(path/'transcript.log').read_bytes())
        metadata['transcript_version']=99
        (path/'metadata.json').write_text(json.dumps(metadata))
        self.assertNotEqual(self.cli('show',path.name).returncode,0)
        self.assertEqual(self.cli('search','after').returncode,2)

if __name__=='__main__': unittest.main(verbosity=2)
