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
    ap.add_argument("--flaky", type=int, default=0,
                    help="answer every N-th ranged GET after the first with a 503, once per range")
    ap.add_argument("--overwrite-after", type=int, default=0,
                    help="after N GETs, serve every object as a new version (a new ETag)")
    args = ap.parse_args()
    lock = threading.Lock()
    gets = [0]
    root_real = os.path.realpath(args.root)

    def inside(*parts):
        """A path under --root, or None for one that would leave it."""
        full = os.path.realpath(os.path.join(root_real, *parts))
        return full if full == root_real or full.startswith(root_real + os.sep) else None
    failed = set()

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

        def etag_of(self, full, version=1):
            st = os.stat(full)
            return '"' + hashlib.md5(f"{full}:{st.st_mtime_ns}:{st.st_size}:{version}".encode()).hexdigest() + '"'

        def list_bucket(self, bucket, query):
            """S3 ListObjectsV2, every key at once."""
            from urllib.parse import parse_qs
            from xml.sax.saxutils import escape
            q = {k: v[0] for k, v in parse_qs(query, keep_blank_values=True).items()}
            prefix = q.get("prefix", "")
            root = inside(bucket)
            if root is None:
                return self.refuse(400)
            keys = []
            for d, dirs, files in os.walk(root):
                dirs[:] = [x for x in dirs if x != ".uploads"]
                for f in files:
                    full = os.path.join(d, f)
                    key = os.path.relpath(full, root).replace(os.sep, "/")
                    if key.startswith(prefix):
                        keys.append((key, full))
            keys.sort()
            items = "".join(
                f"<Contents><Key>{escape(k)}</Key><LastModified>"
                f"{__import__('datetime').datetime.fromtimestamp(os.stat(full).st_mtime, __import__('datetime').timezone.utc).strftime('%Y-%m-%dT%H:%M:%S.000Z')}"
                f"</LastModified><ETag>{escape(self.etag_of(full))}</ETag><Size>{os.path.getsize(full)}</Size>"
                f"<StorageClass>STANDARD</StorageClass></Contents>"
                for k, full in keys)
            xml = (f"<?xml version=\"1.0\" encoding=\"UTF-8\"?><ListBucketResult>"
                   f"<Name>{escape(bucket)}</Name><Prefix>{escape(prefix)}</Prefix><KeyCount>{len(keys)}</KeyCount>"
                   f"<MaxKeys>1000</MaxKeys><IsTruncated>false</IsTruncated>{items}</ListBucketResult>").encode()
            self.reply(200, xml)
            self.record("LIST", len(xml))

        def serve(self, head):
            full, parts = self.target()
            if args.private and not self.headers.get("Authorization") and not parts.query:
                return self.refuse(403)
            bucket_only = unquote(parts.path).strip("/")
            if "list-type=2" in parts.query and "/" not in bucket_only:
                return self.list_bucket(bucket_only, parts.query)
            if full is None:
                return self.refuse(404)
            size = os.path.getsize(full)
            st = os.stat(full)
            with lock:
                gets[0] += 1
                n = gets[0]
            version = 2 if args.overwrite_after and n > args.overwrite_after else 1
            etag = self.etag_of(full, version)
            asked = self.headers.get("Range")
            if args.flaky and asked and n > 1 and n % args.flaky == 0:
                # Once per range: a retry of it gets through.
                with lock:
                    first = (full, asked) not in failed
                    failed.add((full, asked))
                if first:
                    return self.refuse(503)
            wanted = self.headers.get("If-Match")
            if wanted and wanted != etag:
                return self.refuse(412)
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
                d = inside(".uploads", q["uploadId"])
                if d is None:
                    return self.refuse(400)
                os.makedirs(d, exist_ok=True)
                with open(os.path.join(d, "%08d" % int(q["partNumber"])), "wb") as f:
                    f.write(data)
            else:
                full = inside(rel)
                if full is None:
                    return self.refuse(400)
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
                d, full = inside(".uploads", q["uploadId"]), inside(rel)
                if d is None or full is None:
                    return self.refuse(400)
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
