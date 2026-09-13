"""Write every POST body Alertmanager sends to a numbered file."""
import http.server
import itertools
import pathlib
import sys

OUT = pathlib.Path(sys.argv[2])
COUNTER = itertools.count(1)


class Handler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):  # noqa: N802
        body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        (OUT / f"raw-webhook-{next(COUNTER):02}.json").write_bytes(body)
        self.send_response(200)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def log_message(self, *args):
        pass


http.server.HTTPServer(("127.0.0.1", int(sys.argv[1])), Handler).serve_forever()
