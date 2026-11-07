import argparse
import hashlib
import http.server
import json
import os
from pathlib import Path
import signal
import socketserver
import subprocess
import tempfile
import threading


def arguments():
    parser = argparse.ArgumentParser(description="Run one isolated native-agent hook smoke check")
    parser.add_argument("agent", choices=["devin", "goose", "copilot", "antigravity", "kilo", "cursor"])
    parser.add_argument("binary", type=Path)
    parser.add_argument("--hls", type=Path, default=Path("target/debug/hooklinesinker"))
    parser.add_argument("--model", default="openrouter/openai/gpt-4.1-mini")
    parser.add_argument("--root-parent", type=Path, default=Path("/tmp"))
    parser.add_argument("--prepare-only", action="store_true", help="Install hooks and inspect version without a model turn")
    return parser.parse_args()


def environment(root, agent, inherited):
    env = {"PATH": "/usr/bin:/bin:/usr/sbin:/sbin", "LANG": "en_US.UTF-8"}
    roots = {
        "HOME": "home", "XDG_CONFIG_HOME": "config", "XDG_DATA_HOME": "data",
        "XDG_STATE_HOME": "state", "XDG_CACHE_HOME": "cache", "TMPDIR": "tmp",
        "XDG_RUNTIME_DIR": "runtime", "GOOSE_PATH_ROOT": "goose",
        "COPILOT_HOME": "copilot", "KILO_CONFIG_DIR": "config/kilo", "KILO_TEST_HOME": "home",
    }
    for key, part in roots.items():
        path = root / part
        path.mkdir(parents=True, exist_ok=True, mode=0o700)
        env[key] = str(path)
    credential_keys = {
        "devin": [],
        "cursor": ["CURSOR_API_KEY"],
        "goose": ["GOOSE_PROVIDER", "GOOSE_MODEL", "OPENAI_API_KEY", "ANTHROPIC_API_KEY", "GOOGLE_API_KEY", "OPENROUTER_API_KEY"],
        "copilot": ["GH_TOKEN", "GITHUB_TOKEN", "COPILOT_GITHUB_TOKEN"],
        "antigravity": ["GOOGLE_API_KEY", "GEMINI_API_KEY"],
        "kilo": ["OPENROUTER_API_KEY", "OPENAI_API_KEY", "ANTHROPIC_API_KEY", "GOOGLE_API_KEY", "GEMINI_API_KEY"],
    }
    env.update({key: inherited[key] for key in ["SSL_CERT_FILE", *credential_keys[agent]] if key in inherited})
    if agent == "kilo":
        env.update(KILO_DISABLE_PROJECT_CONFIG="true", KILO_DISABLE_DEFAULT_PLUGINS="true")
    return env


def prompt_arguments(agent, root, model):
    prompt = "Reply with OK. Do not use tools."
    return {
        "devin": ["--respect-workspace-trust", "false", "--print", prompt],
        "goose": ["run", "--text", prompt],
        "copilot": ["--no-auto-update", "--disable-builtin-mcps", "--no-custom-instructions", "-p", prompt],
        "antigravity": ["--log-file", str(root / "cli.log"), "--print-timeout", "15s", "-p", prompt],
        "kilo": ["run", "--format", "json", "--model", model, prompt],
        "cursor": ["--trust", "--print", "--output-format", "json", prompt],
    }[agent]


def stop_process(process, timeout):
    errors = []
    for sig in (signal.SIGTERM, signal.SIGKILL):
        try:
            os.killpg(process.pid, sig)
        except ProcessLookupError:
            pass
        except OSError as error:
            errors.append(str(error))
        try:
            stdout, stderr = process.communicate(timeout=timeout)
            return stdout, stderr, errors
        except subprocess.TimeoutExpired as error:
            stdout = (error.output or b"").decode("utf-8", errors="replace")
            stderr = (error.stderr or b"").decode("utf-8", errors="replace")
    process.stdout.close()
    process.stderr.close()
    errors.append("Capture pipes remained open after process-group cleanup")
    try:
        process.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        errors.append("Process did not exit after process-group cleanup")
    return stdout, stderr, errors


def run(command, env, cwd, timeout=45, cleanup_timeout=3):
    result = {"command": [str(arg) for arg in command], "pid": None, "exit": None,
              "timedOut": False, "interrupted": False, "stdout": "", "stderr": ""}
    try:
        process = subprocess.Popen(command, env=env, cwd=cwd, stdout=subprocess.PIPE,
                                   stderr=subprocess.PIPE, text=True, encoding="utf-8",
                                   errors="replace", start_new_session=True)
    except OSError as error:
        return {**result, "launchError": str(error)}
    result["pid"] = process.pid
    try:
        stdout, stderr = process.communicate(timeout=timeout)
    except (subprocess.TimeoutExpired, KeyboardInterrupt, OSError) as error:
        result["timedOut"] = isinstance(error, subprocess.TimeoutExpired)
        result["interrupted"] = isinstance(error, KeyboardInterrupt)
        if isinstance(error, OSError):
            result["captureError"] = str(error)
        stdout, stderr, errors = stop_process(process, cleanup_timeout)
        if errors:
            result["cleanupErrors"] = errors
    return {**result, "exit": process.returncode, "stdout": stdout, "stderr": stderr}


def failed(result):
    return (result["exit"] != 0 or result["timedOut"] or result["interrupted"]
            or bool(result.get("cleanupErrors")) or "captureError" in result)


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


class LoopbackServer(http.server.ThreadingHTTPServer):
    def server_bind(self):
        socketserver.TCPServer.server_bind(self)
        self.server_name = "localhost"
        self.server_port = self.server_address[1]


def main():
    args = arguments()
    binary, hls = args.binary.resolve(strict=True), args.hls.resolve(strict=True)
    root = Path(tempfile.mkdtemp(prefix="hls-native-smoke-", dir=args.root_parent.resolve(strict=True)))
    cwd = root / "project"
    cwd.mkdir()
    env = environment(root, args.agent, os.environ)
    events = []

    class Sink(http.server.BaseHTTPRequestHandler):
        def do_POST(self):
            event = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            events.append(event)
            self.send_response(204)
            self.end_headers()

        def log_message(self, *_args):
            pass

    results = []
    server = LoopbackServer(("127.0.0.1", 0), Sink)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        commands = [
            [hls, "install", "--consumer", "native-smoke", "--sink", f"http://127.0.0.1:{server.server_port}/events"],
            [hls, "hooks", "install", "--agent", args.agent],
            [hls, "hooks", "status", "--agent", args.agent, "--json"],
            [binary, "version" if args.agent == "devin" else "--version"],
        ]
        if not args.prepare_only:
            commands.append([binary, *prompt_arguments(args.agent, root, args.model)])
        for command in commands:
            result = run(command, env, cwd)
            results.append(result)
            if failed(result):
                break
        if not any(result["interrupted"] for result in results):
            results.append(run([hls, "sessions", "--json"], env, cwd))
    finally:
        results.append(run([hls, "uninstall", "--consumer", "native-smoke"], env, cwd))
        server.shutdown()
        server.server_close()
        thread.join()
        report = {"agent": args.agent, "prepareOnly": args.prepare_only, "root": str(root),
                  "binarySha256": sha256(binary),
                  "hlsSha256": sha256(hls),
                  "results": results, "events": events}
        (root / "result.json").write_text(json.dumps(report, indent=2))
        print(root / "result.json")
        print(f"Captured {len(events)} normalized events; inspect exit codes and lifecycle before judging support.")
    if any(result["interrupted"] for result in results):
        return 130
    return int(any(failed(result) for result in results))


if __name__ == "__main__":
    raise SystemExit(main())
