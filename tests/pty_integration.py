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
import shutil
import uuid
import struct
import subprocess
import sys
import tempfile
import termios
import time
import unittest

BINARY = str(Path(__file__).resolve().parents[1] / 'target/debug/termlog')
def session_file(path, name):
    if name == 'transcript.log':
        return path
    return path.with_suffix('.metadata.json' if name == 'metadata.json' else '.cast')

def session_id(path):
    return path.stem.rsplit('_', 1)[1]

def remove_session(path):
    for name in ('transcript.log', 'events.cast', 'metadata.json'):
        file = session_file(path, name)
        if file.exists():
            file.unlink()

def copy_session(source, target):
    target.parent.mkdir(parents=True, exist_ok=True)
    for name in ('transcript.log', 'events.cast', 'metadata.json'):
        source_file = session_file(source, name)
        if source_file.exists():
            shutil.copyfile(source_file, session_file(target, name))

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
        paths=list(self.root.glob('termlog/*_*.log'))
        assert len(paths)==1,paths
        return paths[0]
    def metadata(self): return json.loads(session_file(self.path(), 'metadata.json').read_text())
    def events(self): return [json.loads(line) for line in session_file(self.path(), 'events.cast').read_text().splitlines()[1:]]

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
        self.assertNotIn('secret-password',session_file(t.path(), 'events.cast').read_text())
        self.assertFalse(any(e[1]=='i' for e in t.events()))
        self.assertIn('visible-command',''.join(e[2] for e in t.events() if e[1]=='o'))
    def test_full_hidden_input(self):
        t=self.input_session(['--capture-input'])
        self.assertIn('secret-password',''.join(e[2] for e in t.events() if e[1]=='i'))
        self.assertNotIn('secret-password',session_file(t.path(), 'transcript.log').read_text())
    def test_cli_safe_overrides_config(self):
        t=self.input_session(['--no-capture-input'],'capture_input=true\n')
        self.assertFalse(t.metadata()['capture_input'])
        self.assertNotIn('secret-password',session_file(t.path(), 'events.cast').read_text())
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
        self.assertEqual(t.path().stat().st_mode&0o777,0o600)
        for file in t.path().parent.iterdir(): self.assertEqual(file.stat().st_mode&0o777,0o600)
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
        original=session_file(t.path(), 'transcript.log').read_bytes()
        self.assertEqual(self.cli('rebuild',session_id(t.path())).returncode,0)
        self.assertEqual(session_file(t.path(), 'transcript.log').read_bytes(),original)
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
        rebuilt=self.cli('rebuild',session_id(t.path()));self.assertEqual(rebuilt.returncode,0,rebuilt.stderr)
        self.assertTrue(t.metadata()['transcript_complete']);self.assertIsNone(t.metadata()['transcript_error'])
        self.assertEqual(len(session_file(t.path(), 'transcript.log').read_text().splitlines()),3002)
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
        template=t.metadata();remove_session(t.path())
        parent=self.root/'termlog'
        ids=['ffffffff-ffff-4fff-8fff-ffffffffffff','00000000-0000-4000-8000-000000000000','aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa']
        for id,time_string in zip(ids,['2026-09-20T01:00:00Z','2026-09-20T23:00:00Z','2026-09-20T22:00:00Z']):
            path=parent/f'20260920-000000_{id}.log';metadata=dict(template,session_id=id,started_at=time_string)
            session_file(path, 'metadata.json').write_text(json.dumps(metadata))
        result=self.cli('list');self.assertEqual(result.returncode,0)
        self.assertEqual([line.split()[2] for line in result.stdout.decode().splitlines()],[ids[1],ids[2],ids[0]])
    def test_replay_filters_side_effects_and_ignores_resize(self):
        t=self.run_child('print("ok")');self.assertEqual(t.finish(),0);path=t.path()
        header=json.loads(session_file(path, 'events.cast').read_text().splitlines()[0])
        events=[[0,'o','a\x1b]52;c;secret\x07\x1b]0;title\x07\x1bPpayload\x1b\\\x1b[8;55;180t\x1b[31mred\x1b[0m'],[0,'r','180x55'],[0,'x','0']]
        session_file(path, 'events.cast').write_text('\n'.join(json.dumps(e) for e in [header,*events])+'\n')
        replay=self.terminal(['replay',session_id(path)]);self.assertEqual(replay.finish(),0)
        self.assertTrue(replay.restored);self.assertIn(b'\x1b[31mred',replay.data)
        for denied in [b']52',b'title',b'payload',b'\x1b[8;'] :self.assertNotIn(denied,replay.data)

    def test_incomplete_transcript_visibility(self):
        t=self.run_child('print("needle")');self.assertEqual(t.finish(),0);path=t.path()
        for pattern,code in [('needle',0),('absent',1)]:
            self.assertEqual(self.cli('search',pattern).returncode,code)
        metadata=t.metadata();metadata['transcript_complete']=False
        session_file(path, 'metadata.json').write_text(json.dumps(metadata))
        for pattern in ('needle','absent'):
            result=self.cli('search',pattern)
            self.assertEqual(result.returncode,2,result.stderr)
            self.assertIn(session_id(path).encode(),result.stderr)
            self.assertIn(b'Run: termlog rebuild',result.stderr)
            if pattern=='needle':self.assertIn(b'needle',result.stdout)
        shown=self.cli('show',session_id(path))
        self.assertEqual(shown.returncode,0);self.assertIn(b'needle',shown.stdout)
        self.assertIn(b'incomplete',shown.stderr);self.assertIn(session_id(path).encode(),shown.stderr)
        listed=self.cli('list')
        self.assertIn(b'recording=complete transcript=incomplete',listed.stdout)
        session_file(path, 'transcript.log').unlink()
        self.assertEqual(self.cli('search','needle').returncode,2)
        self.assertNotEqual(self.cli('show',session_id(path)).returncode,0)
        metadata['transcript_complete']=True
        session_file(path, 'metadata.json').write_text(json.dumps(metadata))
        self.assertEqual(self.cli('search','absent').returncode,2)
        session_file(path, 'metadata.json').write_text('invalid json')
        result=self.cli('search','absent')
        self.assertEqual(result.returncode,2);self.assertIn(b'metadata.json',result.stderr)

    def test_search_order_and_timestamp_ties(self):
        t=self.run_child('print("needle")');self.assertEqual(t.finish(),0);path=t.path()
        template=t.metadata();template['started_at']='2026-09-20T00:00:00Z'
        session_file(path, 'metadata.json').write_text(json.dumps(template))
        ids=['ffffffff-ffff-4fff-8fff-ffffffffffff','00000000-0000-4000-8000-000000000000','aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa']
        parent=self.root/'termlog'
        for id,date in zip(ids,['2026-09-20T01:00:00Z','2026-09-20T23:00:00Z','2026-09-20T23:00:00Z']):
            session=parent/f'20260920-000000_{id}.log'
            session_file(session, 'metadata.json').write_text(json.dumps(dict(template,session_id=id,started_at=date)))
            session_file(session, 'transcript.log').write_text('2026-09-20 UTC+00:00\n\n00:00:00.000  needle\n')
        result=self.cli('search','--plain','needle');self.assertEqual(result.returncode,0,result.stderr)
        self.assertEqual([line.split(':')[0] for line in result.stdout.decode().splitlines()],[ids[1],ids[2],ids[0],session_id(path)])
        result=self.cli('list');self.assertEqual(result.returncode,0)
        self.assertEqual([line.split()[2] for line in result.stdout.decode().splitlines()],[ids[1],ids[2],ids[0],session_id(path)])

    def test_replay_restores_display_on_exit_and_interrupt(self):
        t=self.run_child('print("ok")');self.assertEqual(t.finish(),0);path=t.path()
        header=json.loads(session_file(path, 'events.cast').read_text().splitlines()[0])
        reset=b'\x1b[0m\x1b[?6l\x1b[?7h\x1b[r\x1b[?25h\x1b[?1049l'
        for stop in ('normal','q','ctrl-c','sigint','sigterm','parser-error'):
            with self.subTest(stop=stop):
                events=[[0,'o','\x1b[?6h\x1b[?7l\x1b[5;10rREADY']]
                if stop=='parser-error':events.append([0,'o'])
                else:events.append([0 if stop=='normal' else 30,'x','0'])
                session_file(path, 'events.cast').write_text('\n'.join(json.dumps(e) for e in [header,*events])+'\n')
                replay=self.terminal(['replay',session_id(path)]);replay.until(b'READY')
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
        original=session_file(path, 'transcript.log').read_bytes()
        rows=original.decode().splitlines()
        self.assertRegex(rows[0],r'^\d{4}-\d{2}-\d{2} UTC[+-]\d{2}:\d{2}$')
        self.assertEqual(rows[1],'')
        self.assertEqual([row[14:] for row in rows[2:]],['one','two','','  日本語','last'])
        for row in rows[2:]:self.assertRegex(row,r'^\d{2}:\d{2}:\d{2}\.\d{3}  ')
        self.assertNotIn('transcript_version',t.metadata())
        self.assertNotIn('schema_version',t.metadata())
        self.assertNotIn('transcript_version',json.loads(session_file(path, 'events.cast').read_text().splitlines()[0])['termlog'])
        shown=self.cli('show',session_id(path));self.assertEqual(shown.returncode,0,shown.stderr)
        self.assertIn(original,shown.stdout)
        self.assertIn(b'recording=complete transcript=complete',shown.stdout)
        result=self.cli('search','-C','1','two|日本語');self.assertEqual(result.returncode,0,result.stderr)
        self.assertEqual(result.stdout.count(session_id(path).encode()),1)
        self.assertIn(original,result.stdout)
        plain=self.cli('search','--plain','-F','日本語');self.assertEqual(plain.returncode,0,plain.stderr)
        self.assertTrue(plain.stdout.startswith((session_id(path)+':6:').encode()))
        self.assertEqual(self.cli('rebuild',session_id(path)).returncode,0)
        self.assertEqual(original,session_file(path, 'transcript.log').read_bytes())

    def test_millisecond_rebuild_show_and_search(self):
        t=self.run_child('print("ok")');self.assertEqual(t.finish(),0);path=t.path();id=t.metadata()['session_id']
        header=json.loads(session_file(path, 'events.cast').read_text().splitlines()[0])
        header['termlog']['started_at']='2026-09-20T23:59:59+09:00'
        events=[[0.123456,'o','before\r\n'],[2.000789,'o','after\r\n'],[0,'x','0']]
        session_file(path, 'events.cast').write_text('\n'.join(json.dumps(e) for e in [header,*events])+'\n')
        rebuilt=self.cli('rebuild',id);self.assertEqual(rebuilt.returncode,0,rebuilt.stderr)
        original=session_file(path, 'transcript.log').read_bytes()
        self.assertEqual(original,b'2026-09-20 UTC+09:00\n\n23:59:59.123  before\n\n2026-09-21 UTC+09:00\n\n00:00:01.124  after\n')
        shown=self.cli('show',id);self.assertEqual(shown.returncode,0,shown.stderr)
        self.assertIn(original,shown.stdout)
        result=self.cli('search','--plain','after');self.assertEqual(result.returncode,0,result.stderr)
        self.assertEqual(result.stdout,(id+':7:2026-09-21T00:00:01.124+09:00\tafter\n').encode())
        self.assertEqual(original,session_file(path, 'transcript.log').read_bytes())

    def test_transcript_cannot_be_disabled(self):
        for setting in ('enabled=false','enabled=true','timestamps="rfc3339"'):
            self.config('[transcript]\n'+setting+'\n')
            result=self.cli('list')
            self.assertNotEqual(result.returncode,0);self.assertIn(b'unknown field',result.stderr)

    def test_metadata_damage_is_local_and_cast_can_rebuild(self):
        t=self.run_child('print("recoverable needle")');self.assertEqual(t.finish(),0)
        path=t.path();original=session_file(path, 'transcript.log').read_bytes();good=t.metadata()
        self.assertEqual(self.cli('search','needle').returncode,0)
        damaged=[]
        for kind in ('broken','missing','wrong-id','no-cast','bad-cast'):
            id=str(uuid.uuid4());copy=path.with_name(f"{path.stem.rsplit('_', 1)[0]}_{id}.log");copy_session(path,copy)
            if kind=='broken':session_file(copy, 'metadata.json').write_text('broken json')
            elif kind=='wrong-id':pass
            else:session_file(copy, 'metadata.json').unlink()
            if kind=='no-cast':session_file(copy, 'events.cast').unlink()
            if kind=='bad-cast':session_file(copy, 'events.cast').write_text('broken cast')
            damaged.append((id,copy,kind))
        empty_id=str(uuid.uuid4());empty=self.root/'termlog'/f'20000101-000000_{empty_id}.log';empty.touch()
        result=self.cli('search','--plain','needle')
        self.assertEqual(result.returncode,2,result.stderr)
        ids=[line.split(':')[0] for line in result.stdout.decode().splitlines()]
        self.assertCountEqual(ids,[good['session_id'],*[id for id,_,_ in damaged]])
        self.assertEqual(self.cli('search','no-matches').returncode,2)
        listed=self.cli('list');self.assertEqual(listed.returncode,0,listed.stderr)
        for id in [good['session_id'],empty_id,*[id for id,_,_ in damaged]]:self.assertIn(id.encode(),listed.stdout)
        self.assertIn(b'recording=unknown transcript=unknown',listed.stdout)
        for id,copy,kind in damaged:
            shown=self.cli('show',id);self.assertEqual(shown.returncode,0,shown.stderr)
            self.assertIn(b'needle',shown.stdout);self.assertIn(id.encode(),shown.stderr)
            metadata_before=session_file(copy, 'metadata.json').read_bytes() if session_file(copy, 'metadata.json').exists() else None
            if kind in ('no-cast','bad-cast'):
                self.assertNotEqual(self.cli('rebuild',id).returncode,0)
                self.assertEqual(original,session_file(copy, 'transcript.log').read_bytes())
            else:
                session_file(copy, 'transcript.log').unlink()
                result=self.cli('rebuild',id);self.assertEqual(result.returncode,0,result.stderr)
                self.assertEqual(original,session_file(copy, 'transcript.log').read_bytes())
                self.assertIn(b'unknown',result.stderr)
            metadata_after=session_file(copy, 'metadata.json').read_bytes() if session_file(copy, 'metadata.json').exists() else None
            self.assertEqual(metadata_before,metadata_after)
        self.assertEqual(self.cli('search','needle').returncode,2)
        self.assertEqual(good,json.loads(session_file(path, 'metadata.json').read_text()))

    def test_flat_layout_and_uuid_resolution(self):
        t=self.run_child('input("layout-ready");print("layout-done")');t.until(b'layout-ready')
        path=t.path();metadata=t.metadata();id=metadata['session_id']
        self.assertRegex(path.name,r'^\d{8}-\d{6}_[0-9a-f-]{36}\.log$')
        self.assertEqual(path.parent,self.root/'termlog')
        self.assertTrue(session_file(path, 'events.cast').is_file())
        self.assertTrue(session_file(path, 'metadata.json').is_file())
        active=self.cli('rebuild',id)
        self.assertNotEqual(active.returncode,0);self.assertIn(b'running session',active.stderr)
        t.send(b'\n');self.assertEqual(t.finish(),0,t.data)
        self.assertEqual(self.cli('show',id[:8]).returncode,0)

if __name__=='__main__': unittest.main(verbosity=2)
