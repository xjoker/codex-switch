#!/usr/bin/env python3
"""Exercise a real Codex CLI against a loopback-only Responses/MCP fixture.

No model service or real credentials are used. The mock emits deterministic tool
calls; this checks transport/configuration, not an LLM's ability to choose tools.
"""

import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def mcp_server():
    for line in sys.stdin:
        request = json.loads(line)
        if "id" not in request:
            continue
        method = request.get("method")
        if method == "initialize":
            result = {
                "protocolVersion": request["params"]["protocolVersion"],
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "provider-audit", "version": "1"},
            }
        elif method == "tools/list":
            result = {"tools": [{
                "name": "echo", "description": "Return a deterministic audit marker.",
                "inputSchema": {"type": "object", "properties": {}, "additionalProperties": False},
                "annotations": {"readOnlyHint": True, "destructiveHint": False, "openWorldHint": False},
            }]}
        elif method == "tools/call":
            result = {"content": [{"type": "text", "text": "AUDIT_MCP_OK"}]}
        else:
            result = {}
        print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)


def tool_specs(body):
    found = []

    def visit(value, namespace=None):
        if isinstance(value, dict):
            if value.get("type") == "namespace":
                namespace = value.get("name")
            if value.get("type") in ("function", "custom"):
                found.append((namespace, value))
            for child in value.values():
                visit(child, namespace)
        elif isinstance(value, list):
            for child in value:
                visit(child, namespace)

    visit(body.get("tools", []))
    for item in body.get("input", []):
        if item.get("type") == "additional_tools":
            visit(item)
    return found


def message(text):
    return {"type": "message", "id": "msg_audit", "role": "assistant",
            "content": [{"type": "output_text", "text": text}]}


class Fixture:
    def __init__(self, child_model=None):
        self.requests = []
        self.lock = threading.Lock()
        self.child_model = child_model

    def output(self, body, headers):
        if len(self.requests) > 16:
            return message("AUDIT_REQUEST_LIMIT_EXCEEDED")
        # A fork includes the parent history, so use the protocol's child marker.
        if headers.get("x-openai-subagent"):
            return message("AUDIT_CHILD_OK")
        inputs = json.dumps(body.get("input", []))
        specs = tool_specs(body)
        if "audit_mcp" not in inputs:
            selected = next(((ns, spec) for ns, spec in specs
                             if "echo" in spec.get("name", "")), None)
            if selected:
                return self.call("audit_mcp", selected, {})
        if "audit_spawn" not in inputs:
            selected = next(((ns, spec) for ns, spec in specs
                             if spec.get("name") == "spawn_agent"), None)
            if selected:
                properties = selected[1].get("parameters", {}).get("properties", {})
                args = {"message": "Return AUDIT_CHILD_OK. Do not call any tools."}
                if "task_name" in properties:
                    args["task_name"] = "audit_child"
                if "fork_turns" in properties:
                    args["fork_turns"] = "none"
                if self.child_model:
                    args["model"] = self.child_model
                return self.call("audit_spawn", selected, args)
        if "audit_wait" not in inputs:
            selected = next(((ns, spec) for ns, spec in specs
                             if spec.get("name") in ("wait", "wait_agent")), None)
            if selected:
                properties = selected[1].get("parameters", {}).get("properties", {})
                args = {}
                ids_key = next((key for key in ("targets", "agent_ids", "ids") if key in properties), None)
                if ids_key:
                    for item in body.get("input", []):
                        if item.get("type") == "function_call_output" and item.get("call_id") == "audit_spawn":
                            output = item.get("output", "{}")
                            if isinstance(output, str):
                                result = json.loads(output)
                                if result.get("agent_id"):
                                    args[ids_key] = [result["agent_id"]]
                    if not args.get(ids_key):
                        return message("AUDIT_SPAWN_FAILED")
                if "timeout_ms" in properties:
                    args["timeout_ms"] = 10000
                return self.call("audit_wait", selected, args)
        return message("AUDIT_PARENT_OK")

    @staticmethod
    def call(call_id, selected, arguments):
        namespace, spec = selected
        item = {"type": "function_call", "call_id": call_id,
                "name": spec["name"], "arguments": json.dumps(arguments)}
        if namespace:
            item["namespace"] = namespace
        return item


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--codex", required=True, type=Path)
    parser.add_argument("--switch", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--catalog", choices=("generic", "native"), default="generic")
    parser.add_argument("--child-model", action="store_true", help="Select a second saved model for the child")
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    fixture = Fixture("audit-child-model" if args.child_model else None)

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_GET(self):
            catalog = {"data": [{"id": "audit-model", "object": "model"}]}
            if args.catalog == "native":
                catalog = {"models": [{
                    "slug": "audit-model", "display_name": "Audit model",
                    "model_messages": {"instructions_template": "Complete the isolated capability audit."},
                    "supported_reasoning_levels": [], "context_window": 32768,
                    "input_modalities": ["text", "image"], "apply_patch_tool_type": "freeform",
                    "multi_agent_version": "v2", "use_responses_lite": True,
                }]}
            if args.child_model:
                if args.catalog == "native":
                    catalog["models"].append(dict(catalog["models"][0], slug="audit-child-model"))
                else:
                    catalog["data"].append({"id": "audit-child-model", "object": "model"})
            body = json.dumps(catalog).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))))
            headers = dict((key.lower(), value) for key, value in self.headers.items())
            with fixture.lock:
                fixture.requests.append({"path": self.path, "headers": headers, "body": body})
                (args.output / "requests.json").write_text(json.dumps(fixture.requests, indent=2), encoding="utf-8")
            if "input" not in body:
                payload = json.dumps({"error": {"message": "input is required"}}).encode()
                self.send_response(400)
                self.send_header("Content-Type", "application/json")
                self.end_headers()
                self.wfile.write(payload)
                return
            item = fixture.output(body, headers)
            events = [
                {"type": "response.created", "response": {"id": "resp_audit"}},
                {"type": "response.output_item.done", "item": item},
                {"type": "response.completed", "response": {"id": "resp_audit",
                 "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}}},
            ]
            payload = "".join("data: " + json.dumps(event) + "\n\n" for event in events).encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        with tempfile.TemporaryDirectory(prefix="provider-capability-") as temp:
            root = Path(temp)
            codex_home = root / ".codex"
            codex_home.mkdir()
            bin_dir = root / "bin"
            bin_dir.mkdir()
            if os.name == "nt":
                (bin_dir / "codex.cmd").write_text(f'@echo off\n"{args.codex.resolve()}" %*\n')
            else:
                (bin_dir / "codex").symlink_to(args.codex.resolve())
            env = {key: value for key, value in os.environ.items()
                   if not key.upper().startswith(("CODEX_", "CS_", "OPENAI_"))
                   and key.upper() not in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY")}
            env.update(HOME=str(root), USERPROFILE=str(root), CODEX_HOME=str(codex_home),
                       CODEX_SWITCH_HOME=str(root / ".codex-switch"), NO_PROXY="127.0.0.1,localhost",
                       PATH=str(bin_dir) + os.pathsep + env.get("PATH", ""))
            config = '\n'.join([
                'web_search = "disabled"',
                '[features]',
                'plugins = false',
                'apps = false',
                '[mcp_servers.audit]',
                'default_tools_approval_mode = "approve"',
                'command = ' + json.dumps(sys.executable),
                'args = ' + json.dumps([str(Path(__file__).resolve()), "--mcp"]),
            ]) + '\n'
            (codex_home / "config.toml").write_text(config, encoding="utf-8")

            def run(name, command, stdin=None):
                stdout_path = args.output / f"{name}.stdout"
                stderr_path = args.output / f"{name}.stderr"
                with stdout_path.open("w", encoding="utf-8") as stdout, stderr_path.open("w", encoding="utf-8") as stderr:
                    process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=stdout,
                                               stderr=stderr, text=True, encoding="utf-8", env=env, cwd=root)
                    try:
                        process.communicate(stdin, timeout=45)
                    except subprocess.TimeoutExpired:
                        if os.name == "nt":
                            subprocess.run(["taskkill", "/PID", str(process.pid), "/T", "/F"],
                                           capture_output=True, timeout=10)
                        else:
                            process.terminate()
                        process.wait(timeout=10)
                        raise
                result = subprocess.CompletedProcess(command, process.returncode,
                            stdout_path.read_text(encoding="utf-8", errors="replace"),
                            stderr_path.read_text(encoding="utf-8", errors="replace"))
                if result.returncode:
                    raise RuntimeError(f"{name} exited {result.returncode}: {result.stderr[-2000:]}")
                return result

            switch = str(args.switch.resolve())
            run("add", [switch, "provider", "add", "audit", "--base-url",
                        f"http://127.0.0.1:{server.server_port}/v1", "--fetch-models",
                        "--allow-insecure-http", "--metadata-fallback", "none",
                        "--set", 'model_providers.audit.http_headers.X-Audit-Key="audit-header-secret"',
                        "--api-key-stdin"], "audit-fake-key\n")
            run("probe", [switch, "--json", "provider", "probe", "audit"])
            before_diagnose = len(fixture.requests)
            diagnostic = json.loads(run("diagnose", [switch, "--json", "provider", "diagnose", "audit",
                "--child-model", "audit-child-model" if args.child_model else "audit-model"]).stdout)
            diagnostic_offline = len(fixture.requests) == before_diagnose and not diagnostic["network_checked"]
            result = run("launch", [switch, "launch", "audit", "--", "exec",
                         "--skip-git-repo-check", "--json", "-s", "read-only",
                         "Run the deterministic local audit."])
            (args.output / "requests.json").write_text(json.dumps(fixture.requests, indent=2), encoding="utf-8")
            model_requests = [r for r in fixture.requests if "input" in r["body"]]
            summary = {
                "codex_version": subprocess.check_output([str(args.codex), "--version"], text=True).strip(),
                "catalog": args.catalog,
                "responses_lite_seen": any(item.get("type") == "additional_tools"
                    for r in model_requests for item in r["body"].get("input", [])),
                "requests": len(model_requests),
                "models": sorted({r["body"].get("model", "") for r in model_requests}),
                "tools": sorted({(ns + "." if ns else "") + spec["name"]
                                 for r in model_requests for ns, spec in tool_specs(r["body"])}),
                "child_requests": sum(bool(r["headers"].get("x-openai-subagent")) for r in model_requests),
                "child_result_returned": any(
                    item.get("type") in ("function_call_output", "agent_message")
                    and "AUDIT_CHILD_OK" in json.dumps(item)
                    for r in model_requests if not r["headers"].get("x-openai-subagent")
                    for item in r["body"].get("input", [])),
                "mcp_roundtrip": any("AUDIT_MCP_OK" in json.dumps(r["body"].get("input", [])) for r in model_requests),
                "parent_completed": "AUDIT_PARENT_OK" in result.stdout,
                "base_config_unchanged": (codex_home / "config.toml").read_text(encoding="utf-8") == config,
                "auth_untouched": not (codex_home / "auth.json").exists(),
                "all_requests_authenticated": all(r["headers"].get("authorization") == "Bearer audit-fake-key" for r in model_requests),
                "all_requests_have_private_header": all(r["headers"].get("x-audit-key") == "audit-header-secret" for r in model_requests),
                "diagnostic_offline": diagnostic_offline,
                "catalog_provenance_correct": diagnostic["catalog"]["source"] == ("native_gateway" if args.catalog == "native" else "generic_gateway"),
                "child_model_routed": any(r["headers"].get("x-openai-subagent")
                    and r["body"].get("model") == ("audit-child-model" if args.child_model else "audit-model")
                    for r in model_requests),
            }
            (args.output / "summary.json").write_text(json.dumps(summary, indent=2), encoding="utf-8")
            print(json.dumps(summary, indent=2))
            required = ("mcp_roundtrip", "child_result_returned", "parent_completed",
                        "base_config_unchanged", "auth_untouched", "all_requests_authenticated",
                        "child_model_routed", "all_requests_have_private_header", "diagnostic_offline", "catalog_provenance_correct")
            if not all(summary[key] for key in required):
                raise RuntimeError("Capability smoke did not satisfy every round-trip assertion")
            if args.catalog == "native" and not summary["responses_lite_seen"]:
                raise RuntimeError("Native fixture did not exercise Responses Lite")
    finally:
        server.shutdown()
        server.server_close()


if __name__ == "__main__":
    if sys.argv[1:] == ["--mcp"]:
        mcp_server()
    else:
        main()
