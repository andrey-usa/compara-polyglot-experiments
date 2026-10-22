#!/usr/bin/env python3
"""A local stand-in for object storage, for the remote-input tests.

Serves the files under --root at /<bucket>/<key> with GET and HEAD and HTTP
ranges (`bytes=a-b`, `bytes=a-`, `bytes=-n`), and the headers object_store
reads an object's metadata from: Content-Length, Content-Range, ETag and
Last-Modified. That is enough to be an S3 endpoint for path-style requests
(AWS_ENDPOINT_URL, signatures not checked) and a plain HTTP server.

--no-ranges answers every GET with the whole object, as servers that ignore
the Range header do. --private refuses requests that carry neither an
Authorization header nor a query string, as a private bucket refuses an
unsigned GET. Every request is appended to --log as one JSON line: method,
path, range asked, bytes sent.

Prints `LISTENING <url>` once it is ready.
"""
import argparse
import email.utils
import hashlib
import json
import os
import re
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import unquote, urlsplit


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--root", required=True)
    ap.add_argument("--port", type=int, default=0)
    ap.add_argument("--log")
    ap.add_argument("--no-ranges", action="store_true")
    ap.add_argument("--private", action="store_true")
    args = ap.parse_args()
    lock = threading.Lock()

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *a):
            pass

        def record(self, asked, sent):
            if args.log:
                with lock, open(args.log, "a") as f:
                    f.write(json.dumps({"method": self.command, "path": self.path,
                                        "range": asked, "sent": sent}) + "\n")

        def target(self):
            parts = urlsplit(self.path)
            rel = unquote(parts.path).lstrip("/")
            full = os.path.realpath(os.path.join(args.root, rel))
            if not full.startswith(os.path.realpath(args.root) + os.sep) or not os.path.isfile(full):
                return None, parts
            return full, parts

        def refuse(self, code):
            body = b"<Error><Code>Refused</Code></Error>"
            self.send_response(code)
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Content-Type", "application/xml")
            self.end_headers()
            if self.command != "HEAD":
                self.wfile.write(body)
            self.record(self.headers.get("Range"), 0)

        def serve(self, head):
            full, parts = self.target()
            if args.private and not self.headers.get("Authorization") and not parts.query:
                return self.refuse(403)
            if full is None:
                return self.refuse(404)
            size = os.path.getsize(full)
            st = os.stat(full)
            etag = '"' + hashlib.md5(f"{full}:{st.st_mtime_ns}:{size}".encode()).hexdigest() + '"'
            asked = self.headers.get("Range")
            start, end, partial = 0, size, False
            if asked and not args.no_ranges and not head:
                m = re.fullmatch(r"bytes=(\d*)-(\d*)", asked.strip())
                if not m or (m.group(1) == "" and m.group(2) == ""):
                    return self.refuse(416)
                a, b = m.group(1), m.group(2)
                if a == "":
                    n = int(b)
                    start, end = max(0, size - n), size
                else:
                    start = int(a)
                    end = size if b == "" else min(size, int(b) + 1)
                if size == 0 or start >= size:
                    self.send_response(416)
                    self.send_header("Content-Range", f"bytes */{size}")
                    self.send_header("Content-Length", "0")
                    self.end_headers()
                    return self.record(asked, 0)
                partial = True
            self.send_response(206 if partial else 200)
            self.send_header("Content-Length", str(end - start))
            if partial:
                self.send_header("Content-Range", f"bytes {start}-{end - 1}/{size}")
            self.send_header("Accept-Ranges", "none" if args.no_ranges else "bytes")
            self.send_header("ETag", etag)
            self.send_header("Last-Modified", email.utils.formatdate(st.st_mtime, usegmt=True))
            self.send_header("Content-Type", "application/octet-stream")
            self.end_headers()
            sent = 0
            if not head:
                with open(full, "rb") as f:
                    f.seek(start)
                    left = end - start
                    while left:
                        chunk = f.read(min(left, 1 << 20))
                        if not chunk:
                            break
                        self.wfile.write(chunk)
                        left -= len(chunk)
                        sent += len(chunk)
            self.record(asked, sent)

        def body(self):
            n = int(self.headers.get("Content-Length") or 0)
            return self.rfile.read(n) if n else b""

        def reply(self, code, payload=b"", ctype="application/xml", etag=None):
            self.send_response(code)
            self.send_header("Content-Length", str(len(payload)))
            self.send_header("Content-Type", ctype)
            if etag:
                self.send_header("ETag", etag)
            self.end_headers()
            if payload:
                self.wfile.write(payload)

        def do_PUT(self):
            parts = urlsplit(self.path)
            q = dict(p.split("=", 1) if "=" in p else (p, "") for p in parts.query.split("&") if p)
            data = self.body()
            if args.private and not self.headers.get("Authorization") and not parts.query:
                return self.refuse(403)
            rel = unquote(parts.path).lstrip("/")
            etag = '"' + hashlib.md5(data).hexdigest() + '"'
            if "uploadId" in q:
                d = os.path.join(args.root, ".uploads", q["uploadId"])
                os.makedirs(d, exist_ok=True)
                with open(os.path.join(d, "%08d" % int(q["partNumber"])), "wb") as f:
                    f.write(data)
            else:
                full = os.path.join(args.root, rel)
                os.makedirs(os.path.dirname(full), exist_ok=True)
                with open(full, "wb") as f:
                    f.write(data)
            self.record("PUT", len(data))
            self.reply(200, etag=etag)

        def do_POST(self):
            parts = urlsplit(self.path)
            q = dict(p.split("=", 1) if "=" in p else (p, "") for p in parts.query.split("&") if p)
            self.body()
            rel = unquote(parts.path).lstrip("/")
            bucket, _, key = rel.partition("/")
            if "uploads" in q:
                upload = hashlib.md5(f"{rel}:{os.getpid()}:{threading.get_ident()}:{len(os.listdir(args.root))}".encode()).hexdigest()
                xml = (f"<?xml version=\"1.0\" encoding=\"UTF-8\"?><InitiateMultipartUploadResult>"
                       f"<Bucket>{bucket}</Bucket><Key>{key}</Key><UploadId>{upload}</UploadId>"
                       f"</InitiateMultipartUploadResult>").encode()
                return self.reply(200, xml)
            if "uploadId" in q:
                d = os.path.join(args.root, ".uploads", q["uploadId"])
                full = os.path.join(args.root, rel)
                os.makedirs(os.path.dirname(full), exist_ok=True)
                with open(full, "wb") as out:
                    for name in sorted(os.listdir(d)):
                        with open(os.path.join(d, name), "rb") as f:
                            out.write(f.read())
                etag = '"' + hashlib.md5(open(full, "rb").read()).hexdigest() + '-1"'
                xml = (f"<?xml version=\"1.0\" encoding=\"UTF-8\"?><CompleteMultipartUploadResult>"
                       f"<Bucket>{bucket}</Bucket><Key>{key}</Key><ETag>{etag}</ETag>"
                       f"</CompleteMultipartUploadResult>").encode()
                return self.reply(200, xml)
            return self.refuse(400)

        def do_DELETE(self):
            self.reply(204)

        def do_GET(self):
            self.serve(head=False)

        def do_HEAD(self):
            self.serve(head=True)

    class Server(ThreadingHTTPServer):
        def handle_error(self, request, client_address):
            # A client that stops reading part-way (it had what it needed)
            # is not an error worth a traceback.
            if not isinstance(sys.exc_info()[1], (BrokenPipeError, ConnectionResetError)):
                super().handle_error(request, client_address)

    server = Server(("127.0.0.1", args.port), Handler)
    print(f"LISTENING http://127.0.0.1:{server.server_address[1]}", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    sys.exit(main())
