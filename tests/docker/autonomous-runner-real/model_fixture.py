import json
from http.server import BaseHTTPRequestHandler, HTTPServer


def sse_payloads(payloads):
    chunks = []
    for payload in payloads:
        chunks.append(f"data: {json.dumps(payload, separators=(',', ':'))}\n\n")
    chunks.append("data: [DONE]\n\n")
    return "".join(chunks).encode("utf-8")


def response_for(body):
    messages = body.get("messages", [])
    transcript = json.dumps(messages).lower()
    has_tool_result = any(message.get("role") == "tool" for message in messages)

    if "phase: implementation" in transcript and not has_tool_result:
        arguments = json.dumps(
            {
                "path": "docker-e2e.txt",
                "content": "written by the real monkey-cli\n",
            },
            separators=(",", ":"),
        )
        return [
            {
                "choices": [
                    {
                        "index": 0,
                        "delta": {
                            "tool_calls": [
                                {
                                    "index": 0,
                                    "id": "real-docker-write",
                                    "type": "function",
                                    "function": {
                                        "name": "write_file",
                                        "arguments": arguments,
                                    },
                                }
                            ]
                        },
                    }
                ]
            },
            {
                "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]
            },
        ]

    if "phase: review" in transcript:
        content = json.dumps(
            {
                "verdict": "pass",
                "findings": [],
                "filesReviewed": ["docker-e2e.txt"],
            },
            separators=(",", ":"),
        )
    else:
        content = "autonomous phase completed"
    return [
        {"choices": [{"index": 0, "delta": {"content": content}}]},
        {"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]},
        {"choices": [], "usage": {"prompt_tokens": 1, "completion_tokens": 1}},
    ]


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.0"

    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        body = json.loads(self.rfile.read(length).decode("utf-8"))
        payload = sse_payloads(response_for(body))
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(payload)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(payload)
        self.wfile.flush()

    def log_message(self, _format, *_args):
        return


HTTPServer(("127.0.0.1", 18080), Handler).serve_forever()
