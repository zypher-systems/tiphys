"""A scripted model, for testing an installed Tiphys without a real provider.

It speaks just enough Chat Completions to drive one turn: asked anything, it
runs `df`, writes what that printed to ~/disk.txt, and says so.

    python3 packaging/test/fake_provider.py 18080
"""
import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer


def sse(chunks):
    return "".join(f"data: {json.dumps(c)}\n\n" for c in chunks) + "data: [DONE]\n\n"


def call(name, args):
    function = {"name": name, "arguments": json.dumps(args)}
    tool_call = {"index": 0, "id": "call_1", "type": "function", "function": function}
    return [
        {"choices": [{"index": 0, "delta": {"tool_calls": [tool_call]}}]},
        {"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]},
    ]


def text(words):
    return [
        {"choices": [{"index": 0, "delta": {"content": words}}]},
        {"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]},
    ]


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def reply(self, content_type, body):
        body = body.encode()
        self.send_response(200)
        self.send_header("content-type", content_type)
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        self.reply("application/json", json.dumps({"data": [{"id": "fake/model"}]}))

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers["content-length"])))
        results = []
        for message in reversed(request["messages"]):
            if message["role"] == "user":
                break
            if message["role"] == "tool":
                results.insert(0, message["content"])
        if not results:
            out = call("shell", {"command": "df -h / | tail -1", "reason": "to see how full the disk is"})
        elif len(results) == 1:
            out = call("write_file", {"path": "~/disk.txt", "content": results[0] + "\n", "reason": "to keep the answer"})
        else:
            out = text("Written to ~/disk.txt.")
        self.reply("text/event-stream", sse(out))


HTTPServer(("127.0.0.1", int(sys.argv[1])), Handler).serve_forever()
