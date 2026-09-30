"""Serves a page after a delay, like a real site over the network."""

import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

DELAY_SECONDS = 2


class SlowPage(BaseHTTPRequestHandler):
    def do_GET(self):
        time.sleep(DELAY_SECONDS)
        body = b"<!doctype html><title>site</title><h1>the real site</h1>"
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        try:
            self.wfile.write(body)
        except BrokenPipeError:
            pass  # the extension aborted this navigation (expected when blocking)

    def log_message(self, *args):
        pass


ThreadingHTTPServer(("127.0.0.1", 8765), SlowPage).serve_forever()
