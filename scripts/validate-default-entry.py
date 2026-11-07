#!/usr/bin/env python3
"""Real PTY entry/account checks, only in a newly created temporary test state.
Usage: python3 scripts/validate-default-entry.py BIN_DIR NEW_OUT
       python3 scripts/validate-default-entry.py --account-fixture BIN_DIR NEW_OUT
No model request, actual user credentials or machine settings are used.
"""
import fcntl,json,os,pathlib,pty,re,select,signal,struct,subprocess,sys,termios,time
fixture=sys.argv[1]=='--account-fixture'
args=sys.argv[2:] if fixture else sys.argv[1:]
bin_dir=pathlib.Path(args[0]).resolve();out=pathlib.Path(args[1]).resolve();out.mkdir()
home=out/'home';home.mkdir(mode=0o700);a=out/'cwd';a.mkdir();b=out/'explicit';b.mkdir()
env={'PATH':os.environ['PATH'],'TERM':'xterm-256color','HOME':str(home),'CAYW_HOME':str(home/'cayw')}
password='synthetic-password-for-isolated-tests';children=[];checks=[]
class Terminal:
 def __init__(self,argv,cwd=a):
  self.master,slave=pty.openpty();fcntl.ioctl(slave,termios.TIOCSWINSZ,struct.pack('HHHH',32,110,0,0))
  def setup():os.setsid();fcntl.ioctl(slave,termios.TIOCSCTTY,0)
  self.child=subprocess.Popen([str(bin_dir/'cayw'),*argv],cwd=cwd,env=env,stdin=slave,stdout=slave,stderr=slave,preexec_fn=setup)
  children.append(self);os.close(slave);self.output=b''
 def send(self,data):os.write(self.master,data)
 def password(self,value):
  # Synchronize on terminal state: libraries may print the prompt immediately
  # before disabling echo. Never use a timing sleep to assume hidden input.
  end=time.monotonic()+10
  while termios.tcgetattr(self.master)[3] & termios.ECHO:
   assert time.monotonic()<end,'password reader did not disable terminal echo'
   time.sleep(.005)
  self.send((value+'\n').encode())
 def until(self,pattern):
  end=time.monotonic()+30
  while not re.search(pattern,self.output.decode(errors='replace')):
   assert time.monotonic()<end,'PTY transition timed out'
   if select.select([self.master],[],[],.1)[0]:
    try:chunk=os.read(self.master,65536)
    except OSError:raise AssertionError('PTY exited before expected state: '+self.output[-1500:].decode(errors='replace'))
    self.output+=chunk
    if b'\x1b[6n' in chunk:self.send(b'\x1b[1;1R')
  return self
 def finish(self):
  end=time.monotonic()+30
  while self.child.poll() is None:
   assert time.monotonic()<end,'PTY exit timed out'
   if select.select([self.master],[],[],.1)[0]:
    try:self.output+=os.read(self.master,65536)
    except OSError:pass
  return self.child.returncode
 def quit(self):self.send(b'\x11');assert self.finish()==0
try:
 if not fixture:
  # No arguments: inherited cwd is the workspace, even without any model setup.
  first=Terminal([]).until(r'connected')
  states=list((home/'cayw'/'workspaces').glob('*/workspace.json'))
  assert len(states)==1 and str(a) in states[0].read_text()
  assert not list((home/'cayw').rglob('launcher-web.json'))
  assert not list((home/'cayw').rglob('web.token'))
  checks.append('bare cayw opens actual TUI at cwd without starting WebUI, browser or web token setup')
  repeated=Terminal([]).until(r'connected');repeated.quit()
  assert first.child.poll() is None
  subprocess.run([str(bin_dir/'cayw'),'models','list'],cwd=a,env=env,check=True,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
  checks.append('second bare command opens another TUI and preserves the original Runtime on exit')
  # Explicit WebUI can attach while the default TUI remains active.
  web=subprocess.Popen([str(bin_dir/'cayw'),'--ui','web','--no-open'],cwd=a,env=env,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,start_new_session=True)
  try:
   assert select.select([web.stdout],[],[],30)[0];ready=json.loads(web.stdout.readline());assert ready['owned_runtime'] is False and ready['access']=='local'
   web.send_signal(signal.SIGINT);assert web.wait(timeout=30)==0;assert first.child.poll() is None
  finally:
   if web.poll() is None:web.send_signal(signal.SIGINT);web.wait(timeout=30)
  checks.append('WebUI is explicitly selected alongside TUI; its shutdown preserves externally owned Runtime')
  first.send(b'\x0eCurrent cwd session\r');first.until('Current cwd session');first.quit()
  explicit=Terminal(['--workspace',str(b)]).until(r'connected');explicit.quit()
  assert any(str(b) in file.read_text() for file in (home/'cayw'/'workspaces').glob('*/workspace.json'))
  compatibility=Terminal(['start','--workspace',str(b)]).until(r'connected');compatibility.quit()
  assert not list((home/'cayw').rglob('connection.json'))
  checks.append('explicit workspace overrides cwd; start alias also defaults to TUI; owned Runtime is cleaned up')
 # Actual optional account command prompts twice, without echo or plaintext storage.
 setup=Terminal(['account','--username','browser-operator']).until('Password \\(hidden\\):')
 setup.password(password);setup.until('Confirm password \\(hidden\\):');setup.password(password)
 assert setup.finish()==0;assert password.encode() not in setup.output
 account=next((home/'cayw'/'workspaces').glob('*/web-account.json'))
 assert account.stat().st_mode & 0o777 == 0o600
 data=account.read_text();assert password not in data and '$argon2id$' in data
 old=account.read_bytes()
 existing=Terminal(['account','--username','browser-operator']);assert existing.finish()!=0;assert account.read_bytes()==old
 checks.append('explicit optional account setup uses hidden PTY input, stores only Argon2id digest at 0600 and preserves an existing account')
 result={'passed':True,'platform':sys.platform,'checks':checks,'account_file':str(account),'workspace':str(a)}
 (out/'summary.json').write_text(json.dumps(result,indent=2)+'\n');print(json.dumps(result))
finally:
 for terminal in children:
  if terminal.child.poll() is None:terminal.child.send_signal(signal.SIGINT);terminal.child.wait(timeout=30)
  try:os.close(terminal.master)
  except OSError:pass
