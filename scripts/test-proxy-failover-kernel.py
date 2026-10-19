#!/usr/bin/env python3
"""Real mihomo isolated topology integration; NOT Rust TemporaryNode end-to-end.

Python 3.10+, standard library only. Run from any directory:
  python D:/Ai/api/agent2api/scripts/test-proxy-failover-kernel.py
Optional: --binary PATH --output NEW_DIRECTORY (existing results never overwritten).
No backend bootstrap, database, existing service, controller or real upstream is used.
Only owned subprocess handles are terminated; all network endpoints are 127.0.0.1:0.
The temporary config is extracted from runtime.rs, not an independently maintained copy.
"""

import argparse
import asyncio
import copy
import datetime
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import socket
import struct
import sys
import tempfile
import time
import traceback
from urllib.parse import urlsplit

ROOT = Path(__file__).resolve().parents[1]
RUNTIME = ROOT / "desktop-tauri/src-tauri/server/src/server/core/sub_kernel/runtime.rs"
HOST = "127.0.0.1"


def sha256(path):
    h = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


class Harness:
    def __init__(self, binary, output):
        self.binary, self.output = binary.resolve(), output.resolve()
        self.start = time.monotonic()
        self.report = {
            "scope": "real-mihomo-isolated-topology-not-rust-end-to-end",
            "rust_temporary_node_executed": False,
            "limitation": "temporary_node depends on global enabled_provider_inputs/kernel_dir; no isolated public test entry used. Does not validate Rust Arc/drop, sticky selection, smart/cooldown or sidecar limits.",
            "started_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
            "script": {"path": str(Path(__file__).resolve()), "sha256": sha256(Path(__file__))},
            "binary": {"path": str(self.binary)},
            "assertions": [], "children": [], "ports": [], "requests": [],
            "mock_connections": [], "errors": [], "configs": [],
        }
        self.log_file = (output / "report.log").open("x", encoding="utf-8", buffering=1)
        self.temp = None
        self.children, self.servers, self.tasks, self.clients = [], [], set(), []
        self.handler_errors, self.route_by_port, self.gates = [], {}, {}
        self.closing = False

    def log(self, event, **fields):
        row = {"t": round(time.monotonic() - self.start, 6), "event": event, **fields}
        self.log_file.write(json.dumps(row, ensure_ascii=False) + "\n")

    def check(self, name, condition, **details):
        row = {"name": name, "passed": bool(condition), "details": details}
        self.report["assertions"].append(row)
        self.log("assertion", **row)
        if not condition:
            raise AssertionError(name + ": " + json.dumps(details))

    async def handler(self, fn, reader, writer):
        task = asyncio.current_task()
        self.tasks.add(task)
        try:
            await asyncio.wait_for(fn(reader, writer), 40)
        except asyncio.CancelledError:
            raise
        except Exception as error:
            if not self.closing:
                self.handler_errors.append(repr(error))
                self.log("handler_error", error=repr(error))
        finally:
            writer.close()
            try:
                await writer.wait_closed()
            except (OSError, ConnectionError):
                pass
            self.tasks.discard(task)

    async def server(self, role, fn):
        server = await asyncio.start_server(lambda r, w: self.handler(fn, r, w), HOST, 0)
        self.servers.append(server)
        port = server.sockets[0].getsockname()[1]
        self.report["ports"].append({"role": role, "host": HOST, "port": port})
        return port

    async def origin(self, reader, writer):
        raw = await reader.readuntil(b"\r\n\r\n")
        request_line = raw.split(b"\r\n", 1)[0].decode("ascii")
        method, target, _ = request_line.split(" ", 2)
        path = urlsplit(target).path
        request_id = path.rsplit("/", 1)[-1]
        route = self.route_by_port.get(writer.get_extra_info("peername")[1])
        if method != "GET" or route is None or not (path.startswith("/sse/") or path.startswith("/short/")):
            raise AssertionError("Unrecognized origin request or bypassed mock proxy")
        self.report["requests"].append({"id": request_id, "route": route, "line": request_line})
        writer.write(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n")
        await writer.drain()
        gate = self.gates.get(request_id)
        sequence = 0
        while True:
            payload = {"id": request_id, "route": route, "seq": sequence}
            writer.write(("data: " + json.dumps(payload) + "\n\n").encode())
            await writer.drain()
            self.log("origin_frame", **payload)
            sequence += 1
            if gate is None or gate.is_set():
                break
            await asyncio.sleep(0.075)
        writer.write(b"data: [DONE]\n\n")
        await writer.drain()

    async def socks(self, route, reader, writer):
        remote = None
        try:
            ver, count = await reader.readexactly(2)
            methods = await reader.readexactly(count)
            if ver != 5 or 0 not in methods:
                raise AssertionError("SOCKS5 no-auth greeting required")
            writer.write(b"\x05\x00")
            await writer.drain()
            ver, cmd, reserved, atyp = await reader.readexactly(4)
            if (ver, cmd, reserved, atyp) != (5, 1, 0, 1):
                raise AssertionError("Only IPv4 SOCKS5 CONNECT allowed; no DNS/external destination")
            host = socket.inet_ntoa(await reader.readexactly(4))
            port = struct.unpack("!H", await reader.readexactly(2))[0]
            if (host, port) != (HOST, self.origin_port):
                raise AssertionError(f"Non-test destination rejected: {host}:{port}")
            upstream, remote = await asyncio.open_connection(HOST, port)
            local_port = remote.get_extra_info("sockname")[1]
            self.route_by_port[local_port] = route
            self.report["mock_connections"].append({"route": route, "host": host, "port": port, "source_port": local_port})
            writer.write(b"\x05\x00\x00\x01" + socket.inet_aton(HOST) + struct.pack("!H", local_port))
            await writer.drain()

            async def pump(source, destination):
                while True:
                    data = await source.read(65536)
                    if not data:
                        return
                    destination.write(data)
                    await destination.drain()

            pumps = [asyncio.create_task(pump(reader, remote)), asyncio.create_task(pump(upstream, writer))]
            try:
                done, _ = await asyncio.wait(pumps, return_when=asyncio.FIRST_COMPLETED)
                for task in done:
                    task.result()
            finally:
                for task in pumps:
                    task.cancel()
                await asyncio.gather(*pumps, return_exceptions=True)
        finally:
            if remote is not None:
                self.route_by_port.pop(remote.get_extra_info("sockname")[1], None)
                remote.close()
                try:
                    await remote.wait_closed()
                except OSError:
                    pass

    def config(self, proxy_port, listen_port):
        # Parse the actual json! block; fail closed on source/schema changes.
        text = RUNTIME.read_text(encoding="utf-8")
        function = text.split("pub async fn temporary_node(", 1)[1]
        block = re.search(r"let config\s*=\s*json!\((\{.*?\})\);", function, re.S)
        if block is None:
            raise AssertionError("temporary_node config block not found")
        definition = {"name": "temporary-exit", "type": "socks5", "server": HOST, "port": proxy_port, "udp": False}
        source = re.sub(r"\[\s*definition\s*\]", json.dumps([definition]), block.group(1))
        source = re.sub(r'("port"\s*:\s*)port\b', lambda m: m.group(1) + str(listen_port), source)
        config = json.loads(source)
        expected = {
            "mode": "rule", "log-level": "silent", "ipv6": False,
            "profile": {"store-selected": False}, "proxies": [definition],
            "rules": ["MATCH,REJECT"],
            "listeners": [{"name": "temporary-in", "type": "mixed", "listen": HOST, "port": listen_port, "proxy": "temporary-exit"}],
        }
        self.check("runtime_config_exact_local_only", config == expected, config=config)
        self.report["runtime"] = {"path": str(RUNTIME), "sha256": sha256(RUNTIME), "config_block_sha256": hashlib.sha256(block.group(1).encode()).hexdigest()}
        return config

    async def spawn(self, label, args, directory):
        log_path = directory / (label + ".log")
        flags = getattr(__import__("subprocess"), "CREATE_NO_WINDOW", 0) if os.name == "nt" else 0
        # Do not inherit system proxy settings even for the version-only child.
        env = {k: v for k, v in os.environ.items() if k.lower() not in {"http_proxy", "https_proxy", "all_proxy", "no_proxy"}}
        with log_path.open("xb") as log:
            process = await asyncio.create_subprocess_exec(str(self.binary), *args, cwd=directory,
                stdin=asyncio.subprocess.DEVNULL, stdout=log, stderr=log, env=env, creationflags=flags)
        record = {"label": label, "pid": process.pid, "command": [str(self.binary), *map(str, args)], "directory": str(directory)}
        self.report["children"].append(record)
        owned = {"process": process, "record": record, "log_path": log_path, "directory": directory}
        self.children.append(owned)
        self.log("child_spawned", **record)
        return owned

    async def sidecar(self, label, proxy_port):
        directory = self.temp / label
        directory.mkdir()
        reservation = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        try:
            reservation.bind((HOST, 0))
            port = reservation.getsockname()[1]
            config = self.config(proxy_port, port)
            config_path = directory / "config.yaml"
            # JSON is a YAML subset accepted by mihomo; values are extracted from Rust.
            config_path.write_text(json.dumps(config, indent=2), encoding="utf-8")
            self.report["configs"].append({"label": label, "config": config, "sha256": sha256(config_path)})
        finally:
            reservation.close()
        child = await self.spawn(label, ["-d", str(directory), "-f", str(config_path)], directory)
        child["port"] = port
        self.report["ports"].append({"role": label, "host": HOST, "port": port})
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            if child["process"].returncode is not None:
                raise AssertionError(f"{label} exited before readiness: {child['process'].returncode}")
            try:
                _, writer = await asyncio.wait_for(asyncio.open_connection(HOST, port), 0.3)
                writer.close()
                await writer.wait_closed()
                self.check(label + "_ready", True, pid=child["process"].pid, port=port)
                return child
            except (OSError, asyncio.TimeoutError):
                await asyncio.sleep(0.05)
        raise AssertionError(label + " readiness timed out")

    async def stream(self, child, request_id, long=True):
        if long:
            self.gates[request_id] = asyncio.Event()
        reader, writer = await asyncio.wait_for(asyncio.open_connection(HOST, child["port"]), 3)
        self.clients.append(writer)
        path = ("/sse/" if long else "/short/") + request_id
        writer.write((f"GET http://{HOST}:{self.origin_port}{path} HTTP/1.1\r\nHost: {HOST}:{self.origin_port}\r\nConnection: close\r\n\r\n").encode())
        await writer.drain()
        headers = await asyncio.wait_for(reader.readuntil(b"\r\n\r\n"), 5)
        self.check(request_id + "_http_sse", headers.startswith(b"HTTP/1.1 200 ") and b"text/event-stream" in headers.lower(), headers=headers.decode())
        stream = {"id": request_id, "frames": [], "done": False, "eof": False}

        async def consume():
            try:
                while True:
                    line = await reader.readline()
                    if not line:
                        stream["eof"] = True
                        break
                    if line.startswith(b"data: "):
                        data = line[6:].strip()
                        if data == b"[DONE]":
                            stream["done"] = True
                        else:
                            if stream["done"]:
                                raise AssertionError("Frame after DONE")
                            frame = json.loads(data)
                            stream["frames"].append(frame)
                            self.log("client_frame", **frame)
            finally:
                writer.close()
                try:
                    await writer.wait_closed()
                except OSError:
                    pass
        stream["task"] = asyncio.create_task(consume())
        return stream

    async def advance(self, streams, counts=None):
        counts = counts or [0] * len(streams)
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            if all(len(s["frames"]) >= n + 2 for s, n in zip(streams, counts)):
                return
            for stream in streams:
                if stream["task"].done():
                    stream["task"].result()
                    raise AssertionError(stream["id"] + " ended before progress barrier")
            await asyncio.sleep(0.025)
        raise AssertionError("SSE progress barrier timed out")

    async def finish(self, stream, route):
        if stream["id"] in self.gates:
            self.gates[stream["id"]].set()
        await asyncio.wait_for(stream["task"], 5)
        frames = stream["frames"]
        self.check(stream["id"] + "_complete_ordered_route", stream["done"] and stream["eof"] and bool(frames)
            and [f["seq"] for f in frames] == list(range(len(frames)))
            and all(f["id"] == stream["id"] and f["route"] == route for f in frames),
            count=len(frames), route=route, done=stream["done"], eof=stream["eof"])

    async def stop(self, child):
        process = child["process"]
        if process.returncode is None:
            process.terminate()  # Exact owned handle on Windows; no name-based kill.
            try:
                await asyncio.wait_for(process.wait(), 5)
            except asyncio.TimeoutError:
                process.kill()
                await asyncio.wait_for(process.wait(), 5)
        else:
            await process.wait()
        child["record"]["returncode"] = process.returncode
        child["record"]["reaped"] = process.returncode is not None
        self.log("child_reaped", pid=process.pid, returncode=process.returncode)

    @staticmethod
    def listening(port):
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
            sock.settimeout(0.15)
            return sock.connect_ex((HOST, port)) == 0

    async def retire(self, child):
        await self.stop(child)
        self.check(child["record"]["label"] + "_port_closed", not self.listening(child["port"]), port=child["port"])
        self.archive_log(child)
        shutil.rmtree(child["directory"])
        self.check(child["record"]["label"] + "_directory_removed", not child["directory"].exists())

    def archive_log(self, child):
        if child["log_path"].exists():
            self.log("child_output", label=child["record"]["label"], text=child["log_path"].read_text(encoding="utf-8", errors="replace"))

    async def scenario(self, prefix, close_new_first):
        old = await self.sidecar(prefix + "-old", self.proxy_a)
        old_streams = [await self.stream(old, prefix + "-old-" + str(i)) for i in range(2)]
        await self.advance(old_streams)
        initial_counts = [len(s["frames"]) for s in old_streams]
        new = await self.sidecar(prefix + "-new", self.proxy_b)
        self.check(prefix + "_independent_pid_port_dir", old["process"].pid != new["process"].pid
            and old["port"] != new["port"] and old["directory"] != new["directory"])
        for i in range(3):
            short = await self.stream(new, prefix + "-new-short-" + str(i), long=False)
            await self.finish(short, "B")
        new_stream = await self.stream(new, prefix + "-new-long")
        await self.advance([new_stream])
        await self.advance(old_streams, initial_counts)
        self.check(prefix + "_old_streams_progress_during_new_requests", all(not s["done"] for s in old_streams),
            before=initial_counts, after=[len(s["frames"]) for s in old_streams])
        if close_new_first:
            await self.finish(new_stream, "B")
            counts = [len(s["frames"]) for s in old_streams]
            await self.retire(new)
            await self.advance(old_streams, counts)
            self.check(prefix + "_old_survives_new_shutdown", old["process"].returncode is None,
                before=counts, after=[len(s["frames"]) for s in old_streams])
            short = await self.stream(old, prefix + "-old-after-new-close", long=False)
            await self.finish(short, "A")
            for stream in old_streams:
                await self.finish(stream, "A")
            await self.retire(old)
        else:
            for stream in old_streams:
                await self.finish(stream, "A")
            count = len(new_stream["frames"])
            await self.retire(old)
            await self.advance([new_stream], [count])
            self.check(prefix + "_new_survives_old_shutdown", new["process"].returncode is None,
                before=count, after=len(new_stream["frames"]))
            short = await self.stream(new, prefix + "-new-after-old-close", long=False)
            await self.finish(short, "B")
            await self.finish(new_stream, "B")
            await self.retire(new)

    async def test(self):
        self.check("binary_exists", self.binary.is_file(), path=str(self.binary))
        self.report["binary"].update(size=self.binary.stat().st_size, sha256=sha256(self.binary))
        with self.binary.open("rb") as binary:
            self.report["binary"]["magic"] = binary.read(2).hex()
        self.temp = Path(tempfile.mkdtemp(prefix="agent2api-kernel-test-"))
        self.report["temporary_root"] = str(self.temp)
        version = await self.spawn("version", ["-d", str(self.temp), "-v"], self.temp)
        await asyncio.wait_for(version["process"].wait(), 5)
        version_text = version["log_path"].read_text(encoding="utf-8", errors="replace").strip()
        self.report["binary"]["version"] = version_text
        self.check("real_binary_version", version["process"].returncode == 0 and "mihomo" in version_text.lower(), output=version_text)
        self.origin_port = await self.server("sse-origin", self.origin)
        self.proxy_a = await self.server("socks-A", lambda r, w: self.socks("A", r, w))
        self.proxy_b = await self.server("socks-B", lambda r, w: self.socks("B", r, w))
        await self.scenario("close-new-first", True)
        await self.scenario("close-old-first", False)
        self.check("all_requests_traversed_local_mock", len(self.report["requests"]) == 14
            and len(self.report["mock_connections"]) == 14
            and all(c["host"] == HOST and c["port"] == self.origin_port for c in self.report["mock_connections"]),
            requests=len(self.report["requests"]), connections=len(self.report["mock_connections"]))
        self.check("no_handler_errors", not self.handler_errors, errors=self.handler_errors)

    async def cleanup(self):
        self.closing = True
        for gate in self.gates.values():
            gate.set()
        for writer in self.clients:
            writer.close()
        for child in self.children:
            try:
                await self.stop(child)
                self.archive_log(child)
            except Exception as error:
                self.report["errors"].append("cleanup child: " + repr(error))
        for server in self.servers:
            server.close()
        for server in self.servers:
            await server.wait_closed()
        remaining = [t for t in asyncio.all_tasks() if t is not asyncio.current_task() and not t.done()]
        for task in remaining:
            task.cancel()
        await asyncio.gather(*remaining, return_exceptions=True)
        if self.temp is not None:
            for attempt in range(30):
                try:
                    shutil.rmtree(self.temp)
                    break
                except FileNotFoundError:
                    break
                except OSError:
                    if attempt == 29:
                        self.report["errors"].append("temporary directory removal failed")
                    await asyncio.sleep(0.1)
        ports = [{**port, "listener_closed": not self.listening(port["port"])} for port in self.report["ports"]]
        cleanup = {
            "children_reaped": all(c["process"].returncode is not None for c in self.children),
            "child_pids": [c["process"].pid for c in self.children],
            "ports": ports, "all_listeners_closed": all(p["listener_closed"] for p in ports),
            "temporary_root_removed": self.temp is None or not self.temp.exists(),
            "pending_async_tasks": len([t for t in asyncio.all_tasks() if t is not asyncio.current_task() and not t.done()]),
            "note": "Checks listening sockets, not TCP TIME_WAIT; never kills pre-existing processes or reclaims their ports.",
        }
        self.report["cleanup"] = cleanup
        self.check("cleanup_complete", cleanup["children_reaped"] and cleanup["all_listeners_closed"]
            and cleanup["temporary_root_removed"] and cleanup["pending_async_tasks"] == 0, **cleanup)

    async def run(self):
        passed = False
        try:
            await asyncio.wait_for(self.test(), 90)
            passed = True
        except BaseException as error:
            self.report["errors"].append(repr(error))
            self.log("test_failed", traceback=traceback.format_exc())
        finally:
            try:
                await self.cleanup()
            except BaseException as error:
                self.report["errors"].append("cleanup: " + repr(error))
                self.log("cleanup_failed", traceback=traceback.format_exc())
            self.report["status"] = "passed" if passed and not self.report["errors"] else "failed"
            self.report["duration_seconds"] = round(time.monotonic() - self.start, 3)
            self.report["finished_at"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
            self.log("result", status=self.report["status"], assertions=len(self.report["assertions"]))
            with (self.output / "report.json").open("x", encoding="utf-8") as output:
                json.dump(self.report, output, indent=2, ensure_ascii=False)
                output.write("\n")
            self.log_file.close()
        print(json.dumps({"status": self.report["status"], "assertions": len(self.report["assertions"]),
            "report": str(self.output / "report.json"), "cleanup": self.report.get("cleanup"), "errors": self.report["errors"]}, indent=2))
        return 0 if self.report["status"] == "passed" else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "data/proxy-kernel/mihomo.exe")
    parser.add_argument("--output", type=Path, default=ROOT / "data/proxy-rotation-optimize/kernel-test")
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    if any((args.output / name).exists() for name in ("report.json", "report.log")):
        parser.error("report.json/report.log already exists; use --output NEW_DIRECTORY to preserve evidence")
    harness = Harness(args.binary, args.output)
    return asyncio.run(harness.run())


if __name__ == "__main__":
    sys.exit(main())
