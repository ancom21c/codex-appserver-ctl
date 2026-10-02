import json
import contextlib
import io
import os
from pathlib import Path
import runpy
import subprocess
import tempfile
import unittest
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[1]


class CommandTests(unittest.TestCase):
    def setUp(self):
        self.cli = runpy.run_path(str(ROOT / 'bin/codex-appserver-ctl'))
        self.globals = self.cli['main'].__globals__

    def test_local_default_and_profile_named_home(self):
        calls = []
        with patch.dict(self.globals, auth_use=lambda *a, **kw: calls.append(a)):
            self.cli['main'](['auth', 'use', 'work'])
            self.cli['main'](['auth', 'use', 'home'])
        self.assertEqual([c[0] for c in calls], ['work', 'home'])

    def test_ssh_destination_parsing(self):
        parse = self.cli['ssh_target']
        self.assertEqual(parse(['auth', 'list', '--target=MY_SERVER']), ('MY_SERVER', ['auth', 'list']))
        self.assertEqual(parse(['--target', 'user@host', 'status']), ('user@host', ['status']))
        for args in [['--target'], ['--target='], ['--target', '-oProxyCommand=bad'],
                     ['--target=a', '--target=b'], ['--target', 'host name']]:
            with self.subTest(args=args), self.assertRaises(self.cli['UserError']):
                parse(args)

    def test_remote_dispatch_and_shell_quoting(self):
        actual_run = subprocess.run
        with tempfile.TemporaryDirectory() as directory:
            executable = Path(directory) / 'codex-appserver-ctl'
            marker = Path(directory) / 'injection'
            for modern in [True, False]:
                help_text = ('auth use [NAME]' if modern else 'auth use TARGET [NAME]') + ' auth login doctor targets logs remote-control pair bootstrap'
                executable.write_text(
                    '#!/usr/bin/env python3\nimport sys,json\n'
                    'if sys.argv[1:]==["--help"]: print(' + repr(help_text) + ')\n'
                    'else: print(json.dumps(sys.argv[1:]))\n'
                )
                executable.chmod(0o755)
                commands = [['auth', 'use', 'work', '--dry-run'], ['auth', 'use'],
                            ['auth', 'save', 'home'], ['status'], ['true', '--dry-run'],
                            ['restart', '--dry-run'], ['stop', '--dry-run'], ['remote-control', 'pair'],
                            ['doctor'], ['targets'], ['logs', '--file', '/tmp/example log'],
                            ['remote-control', 'bootstrap', '--dry-run'], ['auth', 'login', 'work', '--dry-run'],
                            ['auth', 'use', "$(touch " + str(marker) + "); quoted'profile"]]
                for args in commands:
                    captured = []

                    def fake_ssh(command, **kwargs):
                        self.assertEqual(command[0], 'ssh')
                        self.assertEqual(command[-2], 'MY_SERVER')
                        result = actual_run(
                            ['/bin/sh', '-c', command[-1]], text=True, capture_output=True,
                            env={**os.environ, 'PATH': directory + ':' + os.environ['PATH']},
                        )
                        self.assertEqual(result.returncode, 0, result.stderr)
                        if kwargs.get('capture_output'):
                            return result
                        captured.extend(json.loads(result.stdout))
                        return result

                    with self.subTest(modern=modern, args=args), patch.object(subprocess, 'run', fake_ssh):
                        with self.assertRaises(SystemExit) as result:
                            self.cli['main'](args + ['--target', 'MY_SERVER'])
                        self.assertEqual(result.exception.code, 0)
                        expected = list(args)
                        if not modern and args[0] in {'restart', 'stop'}:
                            expected[0:1] = ['home', 'false' if args[0] == 'stop' else 'true']
                        elif not modern and args[0] != 'remote-control':
                            expected.insert(2 if args[0] == 'auth' else 1 if args[0] == 'status' else 0, 'home')
                        self.assertEqual(captured, expected)
                self.assertFalse(marker.exists())

    def test_remote_exit_status(self):
        with patch.object(subprocess, 'run', return_value=subprocess.CompletedProcess([], 255)):
            with self.assertRaises(SystemExit) as result:
                self.cli['main'](['status', '--target', 'unreachable'])
            self.assertEqual(result.exception.code, 255)

    def test_named_control_commands(self):
        calls = []
        with patch.dict(self.globals,
                        run_codex=lambda *a: calls.append(('codex', a)),
                        control_home=lambda *a: calls.append(('control', a))):
            self.cli['main'](['start', '--dry-run'])
            self.cli['main'](['restart', '--dry-run'])
            self.cli['main'](['stop', '--dry-run'])
        self.assertEqual(calls[0][1][0], ['app-server', 'daemon', 'start'])
        self.assertTrue(calls[1][1][0])
        self.assertFalse(calls[2][1][0])
        self.assertTrue(all(c[1][1] if c[0] == 'codex' else c[1][2] for c in calls))

    def test_remote_control_commands(self):
        expected = {
            'bootstrap': ['app-server', 'daemon', 'bootstrap', '--remote-control'],
            'start': ['remote-control', 'start'],
            'stop': ['remote-control', 'stop'],
            'pair': ['remote-control', 'pair'],
            'enable': ['app-server', 'daemon', 'enable-remote-control'],
            'disable': ['app-server', 'daemon', 'disable-remote-control'],
            'status': ['app-server', 'daemon', 'version'],
        }
        calls = []
        with patch.dict(self.globals, run_codex=lambda *a: calls.append(a)):
            for action in expected:
                self.cli['main'](['remote-control', action, '--dry-run'])
            with self.assertRaises(self.cli['UserError']):
                self.cli['main'](['remote-control', 'invalid'])
        self.assertEqual([c[0] for c in calls], list(expected.values()))
        self.assertTrue(all(c[1] for c in calls))

    def test_linux_uses_daemon_without_mac_process_fallback(self):
        calls = []

        def daemon(action, timeout):
            calls.append(action)
            return True, ''

        with patch('sys.platform', 'linux'), patch.dict(
                self.globals, daemon_request=daemon, within_appserver=lambda: False,
                process_table=lambda: self.fail('macOS fallback should not run')):
            self.cli['main'](['restart'])
            self.cli['main'](['stop'])
        self.assertEqual(calls, ['restart', 'stop'])

    def test_self_restart_is_detached(self):
        calls = []
        with patch.dict(self.globals, within_appserver=lambda: True,
                        schedule_control=lambda *a: calls.append(a),
                        daemon_request=lambda *a: self.fail('should detach before stopping')):
            self.cli['main'](['restart'])
        self.assertTrue(calls[0][0])

    def test_auth_rollback_on_restart_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            accounts = home / 'accounts'
            accounts.mkdir()
            auth = home / 'auth.json'
            current = home / 'current'
            auth.write_text('{"account":"old"}')
            auth.chmod(0o600)
            current.write_text('old\n')
            profile = accounts / 'work.json'
            profile.write_text('{"account":"work"}')
            profile.chmod(0o600)

            def fail_restart(*args, **kwargs):
                raise self.cli['UserError']('restart failed')

            with patch.dict(self.globals, CODEX_HOME=home, ACCOUNTS=accounts, AUTH=auth,
                            CURRENT=current, LOCK=home / 'lock', control_home=fail_restart,
                            daemon_status=lambda: {"status": "running"}):
                with self.assertRaises(self.cli['UserError']):
                    self.cli['auth_use']('work', True, False, False, 120, internal=True)
            self.assertFalse(auth.is_symlink())
            self.assertEqual(auth.read_text(), '{"account":"old"}')
            self.assertEqual(current.read_text(), 'old\n')

    def test_new_app_bundle_path_and_parent(self):
        binary = '/Applications/ChatGPT.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex'
        Process = self.cli['Process']
        table = [Process(10, 1, '/Applications/ChatGPT.app/Contents/MacOS/ChatGPT'),
                 Process(11, 10, binary + ' app-server'),
                 Process(12, 10, binary + ' app-server proxy')]
        servers = self.cli['app_servers'](table)
        self.assertEqual([p.pid for p in servers], [11])
        self.assertEqual(self.cli['control_targets'](servers, table), ([10], ['ChatGPT']))

    def test_auth_switch_when_no_server_is_running(self):
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory); accounts = home / 'accounts'; accounts.mkdir()
            profile = accounts / 'work.json'; profile.write_text('{"a":1}'); profile.chmod(0o600)
            auth = home / 'auth.json'; auth.write_text('{"old":true}'); auth.chmod(0o600)
            with patch.dict(self.globals, CODEX_HOME=home, ACCOUNTS=accounts, AUTH=auth,
                            CURRENT=home / 'current', LOCK=home / 'lock',
                            daemon_status=lambda: None, app_servers=lambda *a: [],
                            control_home=lambda *a, **kw: self.fail('no server to restart')):
                with contextlib.redirect_stdout(io.StringIO()) as output:
                    self.cli['auth_use']('work', True, False, False, 120, internal=True)
            self.assertEqual(auth.resolve(), profile.resolve())
            self.assertEqual((home / 'current').read_text(), 'work\n')
            self.assertIn('no-running-app-server', output.getvalue())

    def test_restart_failure_keeps_unmanaged_server_running(self):
        Process = self.cli['Process']
        server = Process(20, 1, '/home/example/.local/bin/codex app-server')
        with patch.dict(self.globals, within_appserver=lambda: False, daemon_status=lambda: None,
                        process_table=lambda: [server], app_servers=lambda *a: [server]), patch('sys.platform', 'darwin'), patch('os.kill') as kill:
            with self.assertRaises(self.cli['UserError']):
                self.cli['control_home'](True, False, False, 120)
            kill.assert_not_called()



class WorkflowTests(unittest.TestCase):
    def setUp(self):
        self.cli = runpy.run_path(str(ROOT / 'bin/codex-appserver-ctl'))
        self.globals = self.cli['main'].__globals__

    def test_targets_includes_wildcards_and_cycle(self):
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            ssh = home / '.ssh'
            ssh.mkdir()
            (ssh / 'config').write_text('Host MY_SERVER *.example !excluded\nInclude conf/*.conf\n')
            (ssh / 'conf').mkdir()
            (ssh / 'conf' / 'hosts.conf').write_text('Host=SERVER_A SERVER_B\nInclude ../config\nInclude ' + str(ssh / 'config') + '\n')
            with patch.dict(self.globals, HOME=home):
                aliases = self.cli['ssh_aliases']([ssh / 'config'])
            self.assertEqual(aliases, ['MY_SERVER', 'SERVER_A', 'SERVER_B'])

    def test_logs_latest_and_explicit_follow(self):
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            older = home / 'appserver-ctl-detached.old.log'
            newer = home / 'appserver-ctl-detached.new.log'
            older.write_text('old'); newer.write_text('new')
            os.utime(older, (1, 1)); os.utime(newer, (2, 2))
            with patch.dict(self.globals, CODEX_HOME=home), patch.object(subprocess, 'run') as run:
                run.return_value = subprocess.CompletedProcess([], 0)
                with contextlib.redirect_stdout(io.StringIO()):
                    self.cli['main'](['logs', '--lines', '12'])
                    self.assertEqual(run.call_args.args[0], ['tail', '-n', '12', '--', str(newer)])
                    self.cli['main'](['logs', '--follow', '--file', str(older)])
                    self.assertEqual(run.call_args.args[0], ['tail', '-n', '100', '-f', '--', str(older)])
                with self.assertRaises(self.cli['UserError']):
                    self.cli['main'](['logs', '--lines', '0'])

    def test_journal_logs(self):
        with patch.object(subprocess, 'run', return_value=subprocess.CompletedProcess([], 0)) as run:
            self.cli['main'](['logs', '--unit', 'codex.service', '--follow', '--lines', '25'])
            self.assertEqual(run.call_args.args[0], ['journalctl', '--user', '--unit=codex.service', '-n', '25', '--no-pager', '-f'])
            with self.assertRaises(self.cli['UserError']):
                self.cli['main'](['logs', '--unit', 'codex.service', '--file', '/tmp/log'])

    def test_login_success_and_failure_preserve_current_auth(self):
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            auth = home / 'auth.json'
            auth.write_text('{"existing":"secret"}'); auth.chmod(0o600)
            accounts = home / 'accounts'
            patches = dict(CODEX_HOME=home, ACCOUNTS=accounts, AUTH=auth,
                           CURRENT=home / 'current', LOCK=home / 'lock', host_codex=lambda: '/fake/codex')
            def login(command, **kwargs):
                self.assertEqual(command[-2:], ['login', '--device-auth'])
                self.assertNotEqual(kwargs['env']['CODEX_HOME'], str(home))
                saved = Path(kwargs['env']['CODEX_HOME']) / 'auth.json'
                saved.write_text('{"new":"credential"}'); saved.chmod(0o600)
                return subprocess.CompletedProcess(command, 0)
            with patch.dict(self.globals, **patches), patch.object(subprocess, 'run', login):
                with contextlib.redirect_stdout(io.StringIO()):
                    self.cli['main'](['auth', 'login', 'work'])
            self.assertEqual((accounts / 'work.json').read_text(), '{"new":"credential"}')
            self.assertEqual(auth.read_text(), '{"existing":"secret"}')
            with patch.dict(self.globals, **patches), patch.object(subprocess, 'run', return_value=subprocess.CompletedProcess([], 1)):
                with self.assertRaises(self.cli['UserError']):
                    self.cli['main'](['auth', 'login', 'failed'])
            self.assertFalse((accounts / 'failed.json').exists())
            self.assertEqual(auth.read_text(), '{"existing":"secret"}')

    def test_login_refuses_active_profile_overwrite(self):
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory); accounts = home / 'accounts'; accounts.mkdir()
            profile = accounts / 'work.json'; profile.write_text('{"a":1}'); profile.chmod(0o600)
            auth = home / 'auth.json'; auth.symlink_to(profile)
            with patch.dict(self.globals, CODEX_HOME=home, ACCOUNTS=accounts, AUTH=auth,
                            host_codex=lambda: '/fake/codex'), patch.object(subprocess, 'run') as run:
                with self.assertRaises(self.cli['UserError']):
                    self.cli['main'](['auth', 'login', 'work', '--force'])
                run.assert_not_called()
            self.assertEqual(profile.read_text(), '{"a":1}')

    def test_login_timeout_keeps_auth_and_cleans_temporary_home(self):
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory); auth = home / 'auth.json'
            auth.write_text('{"old":true}'); auth.chmod(0o600)
            temporary_homes = []

            def timeout(command, **kwargs):
                temporary_homes.append(Path(kwargs['env']['CODEX_HOME']))
                raise subprocess.TimeoutExpired(command, kwargs['timeout'])

            with patch.dict(self.globals, CODEX_HOME=home, ACCOUNTS=home / 'accounts',
                            AUTH=auth, host_codex=lambda: '/fake/codex'), patch.object(subprocess, 'run', timeout):
                with self.assertRaises(self.cli['UserError']):
                    self.cli['main'](['auth', 'login', 'work'])
            self.assertEqual(auth.read_text(), '{"old":true}')
            self.assertFalse((home / 'accounts' / 'work.json').exists())
            self.assertFalse(temporary_homes[0].exists())

    def test_doctor_invalid_permissions_without_secrets(self):
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory); auth = home / 'auth.json'
            auth.write_text('{"token":"DO_NOT_PRINT_ME"}'); auth.chmod(0o644)
            output = io.StringIO()
            with patch.dict(self.globals, CODEX_HOME=home, AUTH=auth, host_codex=lambda: None), contextlib.redirect_stdout(output):
                with self.assertRaises(SystemExit) as result:
                    self.cli['main'](['doctor'])
            self.assertEqual(result.exception.code, 1)
            self.assertIn('FAIL auth', output.getvalue())
            self.assertNotIn('DO_NOT_PRINT_ME', output.getvalue())



class RemoteInstallationTests(unittest.TestCase):
    def setUp(self):
        self.cli = runpy.run_path(str(ROOT / 'bin/codex-appserver-ctl'))
        self.globals = self.cli['main'].__globals__

    def test_missing_remote_prompts_installs_and_retries(self):
        actual_run = subprocess.run
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            commands = []
            def ssh(command, **kwargs):
                commands.append(command)
                if kwargs.get('capture_output'):
                    return subprocess.CompletedProcess(command, 127, '', '')
                result = actual_run(['/bin/sh', '-c', command[-1]],
                                    input=kwargs.get('input'), capture_output=True,
                                    env={**os.environ, 'HOME': str(home)})
                self.assertEqual(result.returncode, 0, result.stderr)
                return result
            with patch.object(subprocess, 'run', ssh), patch('sys.stdin.isatty', return_value=True), patch('builtins.input', return_value='y') as prompt:
                with self.assertRaises(SystemExit) as result:
                    self.cli['main'](['--help', '--target', 'MY_SERVER'])
            self.assertEqual(result.exception.code, 0)
            prompt.assert_called_once()
            self.assertEqual(len(commands), 3)
            self.assertEqual((home / '.local/bin/codex-appserver-ctl').read_bytes(), (ROOT / 'bin/codex-appserver-ctl').read_bytes())
            self.assertIn('ctl="$HOME/.local/bin/codex-appserver-ctl"', commands[-1][-1])

    def test_decline_noninteractive_and_dry_run_do_not_install(self):
        for tty, answer, args in [(True, 'n', ['doctor']), (False, 'y', ['doctor']),
                                  (True, 'y', ['auth', 'login', 'work', '--dry-run'])]:
            with self.subTest(tty=tty, args=args), patch.object(subprocess, 'run', return_value=subprocess.CompletedProcess([], 127, '', '')) as run, patch('sys.stdin.isatty', return_value=tty), patch('builtins.input', return_value=answer) as prompt:
                with self.assertRaises(self.cli['UserError']):
                    self.cli['prepare_remote']('MY_SERVER', args)
                self.assertEqual(run.call_count, 1)
                if not tty or '--dry-run' in args:
                    prompt.assert_not_called()

    def test_unsupported_remote_prompts_for_update(self):
        results = [subprocess.CompletedProcess([], 0, 'auth use TARGET [NAME]', ''),
                   subprocess.CompletedProcess([], 0)]
        with patch.object(subprocess, 'run', side_effect=results) as run, patch('sys.stdin.isatty', return_value=True), patch('builtins.input', return_value='yes'):
            self.assertTrue(self.cli['prepare_remote']('MY_SERVER', ['auth', 'login', 'work']))
            self.assertEqual(run.call_count, 2)
            self.assertEqual(run.call_args.kwargs['input'], (ROOT / 'bin/codex-appserver-ctl').read_bytes())

    def test_install_failure_does_not_retry_command(self):
        results = [subprocess.CompletedProcess([], 127, '', ''), subprocess.CompletedProcess([], 1)]
        with patch.object(subprocess, 'run', side_effect=results) as run, patch('sys.stdin.isatty', return_value=True), patch('builtins.input', return_value='y'):
            with self.assertRaises(self.cli['UserError']):
                self.cli['main'](['auth', 'login', 'work', '--target', 'MY_SERVER'])
            self.assertEqual(run.call_count, 2)

    def test_remote_installer_backs_up_and_refuses_symlink(self):
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory); destination = home / '.local/bin/codex-appserver-ctl'
            destination.parent.mkdir(parents=True)
            destination.write_text('old version')
            source = (ROOT / 'bin/codex-appserver-ctl').read_bytes()
            result = subprocess.run(['python3', '-c', self.cli['REMOTE_INSTALLER']], input=source,
                                    env={**os.environ, 'HOME': str(home)}, capture_output=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            backups = list(destination.parent.glob('codex-appserver-ctl.backup.*'))
            self.assertEqual(backups[0].read_text(), 'old version')
            destination.unlink(); destination.symlink_to(backups[0])
            result = subprocess.run(['python3', '-c', self.cli['REMOTE_INSTALLER']], input=source,
                                    env={**os.environ, 'HOME': str(home)}, capture_output=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(backups[0].read_text(), 'old version')



class InstallerTests(unittest.TestCase):
    def install(self, prefix):
        return subprocess.run(['sh', str(ROOT / 'install.sh'), '--prefix', str(prefix)],
                              text=True, capture_output=True)

    def test_install_update_backup_and_idempotence(self):
        with tempfile.TemporaryDirectory(prefix='ctl test ') as directory:
            prefix = Path(directory)
            destination = prefix / 'bin/codex-appserver-ctl'
            self.assertEqual(self.install(prefix).returncode, 0)
            self.assertEqual(destination.read_bytes(), (ROOT / 'bin/codex-appserver-ctl').read_bytes())
            self.assertTrue(os.access(destination, os.X_OK))
            destination.write_text('old version\n')
            self.assertEqual(self.install(prefix).returncode, 0)
            backups = list(destination.parent.glob('codex-appserver-ctl.backup.*'))
            self.assertEqual(len(backups), 1)
            self.assertEqual(backups[0].read_text(), 'old version\n')
            self.assertEqual(self.install(prefix).returncode, 0)
            self.assertEqual(list(destination.parent.glob('codex-appserver-ctl.backup.*')), backups)

    def test_refuse_symlink(self):
        with tempfile.TemporaryDirectory() as directory:
            prefix = Path(directory)
            (prefix / 'bin').mkdir()
            original = prefix / 'original'
            original.write_text('untouched')
            (prefix / 'bin/codex-appserver-ctl').symlink_to(original)
            self.assertNotEqual(self.install(prefix).returncode, 0)
            self.assertEqual(original.read_text(), 'untouched')


if __name__ == '__main__':
    unittest.main()
