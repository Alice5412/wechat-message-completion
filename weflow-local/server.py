"""WeFlow 兼容的本地只读 HTTP 服务，供微信消息补全程序读取会话与消息。"""

from __future__ import annotations

import hmac
import json
import os
import re
import socket
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, unquote, urlparse

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import wfcore  # noqa: E402

CONFIG_PATH = os.path.join(HERE, "config.json")
LOG_DIR = os.path.join(HERE, "logs")
os.makedirs(LOG_DIR, exist_ok=True)
LOG_PATH = os.path.join(LOG_DIR, "service.log")

_config = wfcore.load_config(CONFIG_PATH)
_store = wfcore.Store(_config)
_lock = threading.Lock()
PROTOCOL_VERSION = 2


def log(message: str) -> None:
    line = f"{time.strftime('%Y-%m-%d %H:%M:%S')} {message}\n"
    try:
        with open(LOG_PATH, "a", encoding="utf-8") as handle:
            handle.write(line)
    except OSError:
        pass


def warm_up() -> None:
    try:
        _store.sessions(limit=1)
        _store.hardlink_index()
        log("warm-up ok")
    except Exception as exc:  # noqa: BLE001
        log(f"warm-up failed: {type(exc).__name__}")


def refresher() -> None:
    while True:
        time.sleep(2.0)
        try:
            _store.refresh_all()
        except Exception as exc:  # noqa: BLE001
            log(f"refresh failed: {type(exc).__name__}")


class Handler(BaseHTTPRequestHandler):
    server_version = "WeFlowLocal/1.0"
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt: str, *args) -> None:  # noqa: A003
        pass

    def log_request(self, code: int | str = "-", size: int | str = "-") -> None:
        path = getattr(self, "path", "").partition("?")[0].rstrip("/")
        if path not in ("/api/v1/health", "/api/v1/sessions", "/api/v1/messages"):
            path = "/api/v1/media" if path.startswith("/api/v1/media/") else "unknown"
        log(f"request {path} {code}")

    # ---------- 工具 ----------
    def authorized(self, query: dict[str, list[str]]) -> bool:
        token = str(_config.get("token") or "").encode("utf-8")
        if not token:
            return False
        header = self.headers.get("Authorization") or ""
        if header.lower().startswith("bearer "):
            if hmac.compare_digest(header[7:].strip().encode("utf-8"), token):
                return True
        supplied = (query.get("access_token") or [""])[0]
        return hmac.compare_digest(supplied.encode("utf-8"), token)

    def send_bytes(self, status: int, body: bytes, content_type: str) -> None:
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        try:
            self.wfile.write(body)
        except (BrokenPipeError, ConnectionAbortedError):
            pass

    def send_json(self, payload: dict, status: int = 200) -> None:
        body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
        self.send_bytes(status, body, "application/json; charset=utf-8")

    def fail(self, status: int, message: str) -> None:
        self.send_json({"success": False, "error": message}, status)

    def do_GET(self) -> None:  # noqa: N802
        parsed = urlparse(self.path)
        query = parse_qs(parsed.query)
        if not self.authorized(query):
            self.fail(401, "invalid token")
            return
        route = parsed.path.rstrip("/")
        try:
            if route == "/api/v1/health":
                self.handle_health()
            elif route == "/api/v1/sessions":
                self.handle_sessions(query)
            elif route == "/api/v1/messages":
                self.handle_messages(query)
            elif route.startswith("/api/v1/media/"):
                self.handle_media(route)
            else:
                self.fail(404, "unknown route")
        except wfcore.AmbiguousTitle:
            self.fail(409, "ambiguous conversation title")
        except Exception as exc:  # noqa: BLE001
            log(f"request failed: {type(exc).__name__}")
            self.fail(500, "local data unavailable")

    # ---------- 路由 ----------
    def handle_health(self) -> None:
        self.send_json(
            {
                "status": "ok",
                "service": "weflow-local",
                "version": PROTOCOL_VERSION,
                "source": "sqlite",
                "wxid": _config.get("wxid", ""),
                "dataRoot": _store.root,
                "time": int(time.time()),
            }
        )

    def handle_sessions(self, query: dict[str, list[str]]) -> None:
        keyword = (query.get("keyword") or [""])[0]
        title = (query.get("title") or [None])[0]
        limit = int((query.get("limit") or ["100"])[0] or 100)
        with _lock:
            sessions = _store.sessions(keyword=keyword, limit=limit, title=title)
        self.send_json(
            {"success": True, "count": len(sessions), "sessions": sessions, "exactTitle": title is not None}
        )

    def handle_messages(self, query: dict[str, list[str]]) -> None:
        talker = (query.get("talker") or [""])[0]
        if not talker:
            self.fail(400, "missing talker")
            return
        limit = int((query.get("limit") or ["50"])[0] or 50)
        with _lock:
            messages = _store.messages(talker, limit)
        self.send_json(
            {
                "success": True,
                "talker": talker,
                "count": len(messages),
                "hasMore": len(messages) >= limit,
                "media": True,
                "messages": messages,
            }
        )

    def handle_media(self, route: str) -> None:
        rest = route[len("/api/v1/media/") :]
        parts = rest.split("/")
        if len(parts) != 2:
            self.fail(400, "bad media path")
            return
        talker = unquote(parts[0])
        match = re.fullmatch(r"([0-9a-fA-F]{32})\.img", parts[1])
        if not talker or len(talker) > 256 or match is None:
            self.fail(400, "bad media identifier")
            return
        md5_value = match.group(1).lower()
        with _lock:
            data = _store.image_bytes(talker, md5_value)
        if not data:
            self.fail(404, "media not found")
            return
        mime = "image/jpeg"
        if data[:8] == b"\x89PNG\r\n\x1a\n":
            mime = "image/png"
        elif data[:6] in (b"GIF87a", b"GIF89a"):
            mime = "image/gif"
        elif data[:4] == b"RIFF" and data[8:12] == b"WEBP":
            mime = "image/webp"
        self.send_bytes(200, data, mime)


class LocalServer(ThreadingHTTPServer):
    def server_bind(self) -> None:
        exclusive = getattr(socket, "SO_EXCLUSIVEADDRUSE", None)
        if exclusive is not None:
            self.allow_reuse_address = False
            self.socket.setsockopt(socket.SOL_SOCKET, exclusive, 1)
        super().server_bind()


def main() -> None:
    port = int(_config.get("port") or 5031)
    try:
        with LocalServer(("127.0.0.1", port), Handler) as server:
            warm_up()
            threading.Thread(target=refresher, daemon=True).start()
            log(f"listening on 127.0.0.1:{port}")
            server.serve_forever()
    except KeyboardInterrupt:
        pass


def stream_style_samples() -> None:
    def emit(record: dict) -> None:
        print(json.dumps(record, ensure_ascii=True))

    with tempfile.TemporaryDirectory(prefix="style-", dir=_config["cache_dir"]) as directory:
        store = wfcore.Store({**_config, "cache_dir": directory})
        try:
            chats = store.private_style_chats()
            emit({"kind": "start", "total": len(chats)})
            for index, texts in enumerate(store.style_texts(chats)):
                emit({"kind": "chat", "index": index})
                for text in texts:
                    emit({"kind": "text", "text": text})
            emit({"kind": "end"})
        finally:
            for snapshot in store.snapshots.values():
                snapshot.close()


if __name__ == "__main__":
    if sys.argv[1:] == ["--style-samples"]:
        stream_style_samples()
    else:
        main()
