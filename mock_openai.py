#!/usr/bin/env python3
"""Mock OpenAI chat server: turn 1 -> tool call exec, turn 2 -> final answer."""
import json, threading
from http.server import BaseHTTPRequestHandler, HTTPServer

calls = {"n": 0}
LOCK = threading.Lock()

class H(BaseHTTPRequestHandler):
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        msgs = body["messages"]
        with LOCK:
            calls["n"] += 1
        if calls["n"] == 1:
            msg = {"role": "assistant", "tool_calls": [{"id": "c1", "type": "function",
                   "function": {"name": "exec", "arguments": json.dumps({"command": "echo smoke-ok"})}}]}
        else:
            tool_out = msgs[-1]["content"]
            msg = {"role": "assistant", "content": f"tool said: {tool_out}"}
        resp = {"choices": [{"index": 0, "message": msg, "finish_reason": "tool_calls" if "tool_calls" in msg else "stop"}]}
        data = json.dumps(resp).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)
    def log_message(self, *a): pass

HTTPServer(("127.0.0.1", 18891), H).serve_forever()
