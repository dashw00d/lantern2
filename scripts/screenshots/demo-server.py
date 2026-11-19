# Tiny HTTP server used by the README demo projects (see demo.sh). Serves a title page and /health.
import argparse, http.server, sys, html, datetime
p = argparse.ArgumentParser()
p.add_argument("--port", type=int, required=True)
p.add_argument("--title", required=True)
p.add_argument("--blurb", default="")
p.add_argument("--accent", default="#6366f1")
a = p.parse_args()
PAGE = f"""<!doctype html><meta charset=utf-8><title>{html.escape(a.title)}</title>
<body style="font-family:system-ui,sans-serif;margin:0;background:#0b1020;color:#e5e7eb;display:grid;place-items:center;height:100vh">
<div style="text-align:center"><div style="width:64px;height:64px;border-radius:16px;background:{a.accent};margin:0 auto 20px"></div>
<h1 style="margin:0 0 8px">{html.escape(a.title)}</h1><p style="color:#9ca3af">{html.escape(a.blurb)}</p></div>"""
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path.startswith("/health"):
            body, ctype = b'{"status":"ok"}', "application/json"
        else:
            body, ctype = PAGE.encode(), "text/html"
        self.send_response(200); self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body)
    def log_message(self, fmt, *args):
        print(f"{datetime.datetime.now():%H:%M:%S} {self.command} {self.path} {args[1]}", flush=True)
print(f"{a.title} ready: listening on http://127.0.0.1:{a.port}", flush=True)
http.server.ThreadingHTTPServer(("127.0.0.1", a.port), H).serve_forever()
