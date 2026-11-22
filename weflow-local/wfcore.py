"""WeChat 4.x 本地只读数据层：解密 SQLCipher 数据库并读取会话与消息。"""

from __future__ import annotations

import hashlib
import json
import os
import re
import shutil
import tempfile
import threading
import time
import unicodedata
from contextlib import suppress
from pathlib import Path
from xml.etree import ElementTree

import sqlcipher3
from Crypto.Cipher import AES

try:
    from compression import zstd
except Exception:  # pragma: no cover
    zstd = None

CT_ZSTD = 4
MAX_IMAGE_BYTES = 2 * 1024 * 1024

SYSTEM_USERS = {
    "notifymessage",
    "weixin",
    "fmessage",
    "floatbottle",
    "medianote",
    "qqmail",
    "tmessage",
    "newsapp",
    "brandsessionholder",
    "brandservicesessionholder",
    "weibo",
    "voipnotify",
}

TEXT_TYPES = {1: None}
MEDIA_LABELS = {
    3: ("[图片]", "image"),
    34: ("[语音]", "voice"),
    43: ("[视频]", "video"),
    47: ("[表情包]", "emoji"),
    48: ("[位置]", "location"),
    42: ("[名片]", "card"),
    50: ("[语音通话]", "call"),
    10000: ("[系统消息]", "system"),
}


def md5_hex(text: str) -> str:
    return hashlib.md5(text.encode("utf-8")).hexdigest()


class Keyring:
    def __init__(self, db_key_hex: str):
        self.key = bytes.fromhex(db_key_hex)
        self._cache: dict[tuple[str, bytes], bytes] = {}

    def raw_key(self, salt: bytes) -> bytes:
        cached = self._cache.get(("v1", salt))
        if cached is None:
            enc = hashlib.pbkdf2_hmac("sha512", self.key, salt, 256000, 32)
            cached = enc + salt
            self._cache[("v1", salt)] = cached
        return cached


class Snapshot:
    """把运行中的微信数据库复制到缓存目录后解密读取，源文件变化则重建。"""

    def __init__(self, src: str, cache_dir: str, keyring: Keyring):
        self.src = src
        self.cache_dir = cache_dir
        self.keyring = keyring
        self.con = None
        self.signature = None
        self.error = None
        self.directory = None

    def _stat(self):
        parts = []
        for suffix in ("", "-wal"):
            path = self.src + suffix
            try:
                info = os.stat(path)
                parts.append((info.st_mtime_ns, info.st_size, info.st_ino))
            except FileNotFoundError:
                parts.append(None)
        return tuple(parts)

    def refresh(self, force: bool = False) -> None:
        try:
            signature = self._stat()
        except OSError as exc:
            self.error = type(exc).__name__
            return
        if signature[0] is None:
            self.error = "数据库不存在"
            return
        if not force and signature == self.signature and self.con is not None:
            self.error = None
            return
        for _ in range(3):
            directory = tempfile.TemporaryDirectory(
                prefix="snapshot-", dir=self.cache_dir, ignore_cleanup_errors=True
            )
            dst = os.path.join(directory.name, os.path.basename(self.src))
            con = None
            try:
                shutil.copyfile(self.src, dst)
                if signature[1] is not None:
                    shutil.copyfile(self.src + "-wal", dst + "-wal")
                if self._stat() != signature:
                    raise OSError("数据库正在写入，快照已重试")
                with open(dst, "rb") as handle:
                    salt = handle.read(16)
                con = sqlcipher3.connect(dst, check_same_thread=False)
                con.execute("PRAGMA cipher_compatibility = 3")
                con.execute(f"PRAGMA key = \"x'{self.keyring.raw_key(salt).hex()}'\"")
                con.execute("SELECT count(*) FROM sqlite_master").fetchone()
            except Exception as exc:  # noqa: BLE001
                if con is not None:
                    with suppress(Exception):
                        con.close()
                directory.cleanup()
                self.error = type(exc).__name__
                try:
                    current = self._stat()
                except OSError:
                    return
                if current == signature:
                    return
                signature = current
                continue
            self.close()
            self.con = con
            self.directory = directory
            self.signature = signature
            self.error = None
            return

    def close(self) -> None:
        if self.con is not None:
            try:
                self.con.close()
            except Exception:  # noqa: BLE001
                pass
            self.con = None
        if self.directory is not None:
            self.directory.cleanup()
            self.directory = None


def decode_text(value, flag: int | None) -> str:
    if value is None:
        return ""
    if isinstance(value, str):
        return value
    data = bytes(value)
    if not data:
        return ""
    if flag and flag & CT_ZSTD and zstd is not None:
        try:
            data = zstd.decompress(data)
        except Exception:  # noqa: BLE001
            pass
    return data.decode("utf-8", "replace")


def parse_xml(text: str):
    body = text.strip()
    start = body.find("<")
    if start < 0:
        return None
    try:
        return ElementTree.fromstring(body[start:])
    except ElementTree.ParseError:
        return None


def attr(root, name: str, default: str = "") -> str:
    if root is None:
        return default
    value = root.attrib.get(name)
    if value is None:
        return default
    return value


def first_text(root, tag: str) -> str:
    if root is None:
        return ""
    node = root.find(tag)
    if node is None or node.text is None:
        return ""
    return node.text.strip()


def find_md5(root) -> str:
    if root is None:
        return ""
    if root.attrib.get("md5"):
        return root.attrib["md5"]
    for node in root.iter():
        if node is not root and node.attrib.get("md5"):
            return node.attrib["md5"]
    return ""


class AmbiguousTitle(ValueError):
    pass


class Store:
    def __init__(self, config: dict):
        self.config = config
        self.root = config["root"]
        self.db_root = os.path.join(self.root, "db_storage")
        self.attach_root = os.path.join(self.root, "msg", "attach")
        self.cache_dir = config["cache_dir"]
        os.makedirs(self.cache_dir, exist_ok=True)
        self.keyring = Keyring(config["db_key"])
        self.lock = threading.RLock()
        self.snapshots: dict[str, Snapshot] = {}
        self.contacts: dict[str, dict] = {}
        self.contacts_signature = None
        self.hardlink: dict[str, list[dict]] = {}
        self.hardlink_signature = None
        self.image_key = config.get("image_key") or {}

    # ---------- 基础设施 ----------
    def path(self, *parts: str) -> str:
        return os.path.join(self.db_root, *parts)

    def snapshot(self, path: str) -> Snapshot:
        snap = self.snapshots.get(path)
        if snap is None:
            snap = Snapshot(path, self.cache_dir, self.keyring)
            self.snapshots[path] = snap
        snap.refresh()
        if snap.error:
            raise RuntimeError(f"{os.path.basename(path)} 读取失败：{snap.error}")
        return snap

    def refresh_all(self) -> None:
        with self.lock:
            for path in list(self.snapshots):
                self.snapshots[path].refresh()

    # ---------- 会话 ----------
    def load_contacts(self) -> dict[str, dict]:
        snap = self.snapshot(self.path("contact", "contact.db"))
        rows = snap.con.execute(
            "SELECT username, remark, nick_name, alias, local_type FROM contact"
        ).fetchall()
        contacts = {}
        for username, remark, nick, alias, local_type in rows:
            if not username:
                continue
            name = (remark or "").strip() or (nick or "").strip() or (alias or "").strip()
            contacts[username] = {
                "name": name or username,
                "nick": (nick or "").strip(),
                "alias": (alias or "").strip(),
                "type": local_type,
            }
        self.contacts = contacts
        self.contacts_signature = snap.signature
        return contacts

    def sessions(self, keyword: str = "", limit: int = 100, title: str | None = None) -> list[dict]:
        def normalized(value: str) -> str:
            return "".join(unicodedata.normalize("NFKC", value).casefold().split())

        def matches_title(username: str, info: dict) -> bool:
            return bool(lookup) and any(
                normalized(value) == lookup
                for value in (username, info.get("name"), info.get("nick"), info.get("alias"))
                if value
            )

        lookup = normalized(title) if title is not None else None
        with self.lock:
            snap = self.snapshot(self.path("session", "session.db"))
            if self.contacts_signature != self.snapshot(self.path("contact", "contact.db")).signature:
                self.load_contacts()
            rows = snap.con.execute(
                "SELECT username, summary, last_timestamp, unread_count, type "
                "FROM SessionTable ORDER BY sort_timestamp DESC"
            ).fetchall()
            contacts = self.contacts
        if lookup is not None:
            matching_contacts = {
                username for username, info in contacts.items() if matches_title(username, info)
            }
            matching_contacts.update(
                username for username, *_ in rows if username and normalized(username) == lookup
            )
            if len(matching_contacts) > 1:
                raise AmbiguousTitle()
        result = []
        for username, summary, last_ts, unread, kind in rows:
            if not username or username in SYSTEM_USERS:
                continue
            if username.endswith("@openim"):
                continue
            info = contacts.get(username) or {}
            display = info.get("name") or username
            if lookup is not None and not matches_title(username, info):
                continue
            result.append(
                {
                    "username": username,
                    "displayName": display,
                    "type": kind,
                    "lastTimestamp": int(last_ts or 0),
                    "unreadCount": int(unread or 0),
                    "summary": summary or "",
                    "isGroup": username.endswith("@chatroom"),
                }
            )
        key = keyword.strip()
        if title is None and key:
            lowered = key.lower()
            matched = [
                item
                for item in result
                if lowered in item["displayName"].lower() or lowered in item["username"].lower()
            ]
            result = matched
        return result[: max(1, min(limit, 90))]

    # ---------- 名字解析 ----------
    def message_dbs(self) -> list[str]:
        folder = self.path("message")
        if not os.path.isdir(folder):
            return []
        names = [name for name in os.listdir(folder) if re.fullmatch(r"message_\d+\.db", name)]
        names.sort(key=lambda n: int(re.findall(r"\d+", n)[0]))
        return [os.path.join(folder, name) for name in names]

    def private_style_chats(self) -> list[str]:
        con = self.snapshot(self.path("session", "session.db")).con
        return list(dict.fromkeys(
            name for (name,) in con.execute(
                "SELECT username FROM SessionTable ORDER BY sort_timestamp DESC"
            )
            if name and "@" not in name and not name.startswith("gh_")
            and name not in SYSTEM_USERS and name != "filehelper"
        ))

    def style_texts(self, chats: list[str]):
        account = self.config.get("wxid", "")
        aliases = {name for name in (account, account.rsplit("_", 1)[0]) if name}
        sources = []
        for path in self.message_dbs():
            con = self.snapshot(path).con
            tables = {
                name for (name,) in con.execute("SELECT name FROM sqlite_master WHERE type='table'")
            }
            if "Name2Id" not in tables:
                continue
            ids = [
                rowid for rowid, name in con.execute("SELECT rowid, user_name FROM Name2Id")
                if name in aliases
            ]
            if ids:
                sources.append((con, tables, ids))

        def read_chat(table: str):
            seen = set()
            for con, tables, ids in sources:
                if table not in tables:
                    continue
                columns = {row[1] for row in con.execute(f'PRAGMA table_info("{table}")')}
                flag = "WCDB_CT_message_content" if "WCDB_CT_message_content" in columns else "0"
                server = "server_id" if "server_id" in columns else "0"
                placeholders = ",".join("?" for _ in ids)
                for body, encoding, server_id in con.execute(
                    f'SELECT message_content, {flag}, {server} FROM "{table}" '
                    f'WHERE (local_type & 4294967295) = 1 AND real_sender_id IN ({placeholders})',
                    ids,
                ):
                    if server_id and server_id in seen:
                        continue
                    if server_id:
                        seen.add(server_id)
                    text = decode_text(body, encoding).strip()
                    if text and len(text) <= 500 and "://" not in text and not text.startswith(("[", "<")):
                        yield text

        for talker in chats:
            yield read_chat("Msg_" + md5_hex(talker))

    # ---------- 消息 ----------
    def messages(self, talker: str, limit: int) -> list[dict]:
        limit = max(1, min(int(limit or 50), 200))
        table = "Msg_" + md5_hex(talker)
        account = self.config.get("wxid", "")
        senders = {name for name in (account, account.rsplit("_", 1)[0]) if name}
        candidates = []
        with self.lock:
            for path in self.message_dbs():
                con = self.snapshot(path).con
                columns = {row[1] for row in con.execute(f'PRAGMA table_info("{table}")')}
                if not columns:
                    continue
                flag_col = "WCDB_CT_message_content" if "WCDB_CT_message_content" in columns else "0"
                server_col = "server_id" if "server_id" in columns else "0"
                rows = con.execute(
                    f'SELECT local_id, local_type, real_sender_id, create_time, message_content, '
                    f'{flag_col}, sort_seq, {server_col} FROM "{table}" '
                    "ORDER BY create_time DESC, sort_seq DESC, local_id DESC LIMIT ?",
                    (limit,),
                ).fetchall()
                name_table = con.execute(
                    "SELECT 1 FROM sqlite_master WHERE type='table' AND name='Name2Id'"
                ).fetchone()
                names = dict(con.execute("SELECT rowid, user_name FROM Name2Id")) if name_table else {}
                candidates.extend((row, names.get(row[2]) or "") for row in rows)

        candidates.sort(
            key=lambda candidate: (candidate[0][3] or 0, candidate[0][6] or 0, candidate[0][0] or 0),
            reverse=True,
        )
        newest = []
        seen = set()
        for row, sender in candidates:
            server_id = int(row[7] or 0)
            if server_id and server_id in seen:
                continue
            if server_id:
                seen.add(server_id)
            newest.append((row, sender))
            if len(newest) == limit:
                break
        result = []
        for (local_id, local_type, _, create_time, content, flag, sort_seq, _), sender in reversed(newest):
            text = decode_text(content, flag)
            label, media_type, media_url = self.describe(local_type, text, talker)
            result.append(
                {
                    "localId": int(local_id or 0),
                    "createTime": int(create_time or 0),
                    "sortSeq": int(sort_seq or 0),
                    "isSend": int(sender in senders),
                    "localType": int(local_type or 0),
                    "senderUsername": sender,
                    "content": label if media_type is None else "",
                    "parsedContent": label,
                    "mediaType": media_type,
                    "mediaUrl": media_url,
                }
            )
        return result

    def describe(self, local_type: int, text: str, talker: str) -> tuple[str, str | None, str | None]:
        local_type = int(local_type or 0)
        kind = local_type & 0xFFFFFFFF
        if kind == 49:
            root = parse_xml(text) if len(text) <= 128 * 1024 and "<!DOCTYPE" not in text else None
            app = None if root is None else (root if root.tag == "appmsg" else root.find("appmsg"))
            app_type = first_text(app, "type") or str(local_type >> 32)
            title = " ".join(first_text(app, "title").split())
            if any(ch in title for ch in "<>") or re.search(
                r"&lt;(?:aeskey|fileuploadtoken|cdnattachurl|appattach)", title
            ):
                title = ""
            title = title[:160]
            label = ("[链接] " + title).rstrip() if app is not None else "[微信消息]"
            if app_type == "57":
                label = "[引用]"
            elif app_type == "19":
                label = "[聊天记录]"
            elif app_type == "6":
                label = ("[文件] " + title).rstrip()
            elif app_type in {"2000", "2001"}:
                label = title or "[转账]"
            elif app_type in {"33", "36"}:
                label = title or "[小程序]"
            return label, None, None
        if kind in (3, 47):
            root = parse_xml(text)
            md5_value = find_md5(root)
            label = "[图片]" if kind == 3 else "[表情包]"
            media = "image" if kind == 3 else "emoji"
            url = None
            if md5_value:
                url = self.media_url(talker, md5_value)
            return label, media, url
        if kind in MEDIA_LABELS:
            label, media = MEDIA_LABELS[kind]
            return label, media, None
        body = text.strip()
        start = body.find("<")
        if start >= 0:
            envelope = re.sub(r"^<\?xml[^?]*\?>\s*", "", body[start:], count=1)
            if re.match(r"<(?:msg|appmsg|sysmsg|img|emoji|voicemsg|videomsg|location)(?=[\s>/])", envelope):
                label = "[系统消息]" if envelope.startswith("<sysmsg") else "[微信消息]"
                return label, None, None
        if re.search(r"(?:<|&lt;)(?:aeskey|fileuploadtoken|cdnattachurl|appattach)", body):
            return "[微信消息]", None, None
        return body or "[消息]", None, None

    # ---------- 图片 ----------
    def media_url(self, talker: str, md5_value: str) -> str | None:
        if next(self.find_dat(talker, md5_value), None) is None:
            return None
        return f"/api/v1/media/{quote_talker(talker)}/{md5_value}.img"

    def hardlink_index(self) -> dict[str, list[dict]]:
        with self.lock:
            snap = self.snapshot(self.path("hardlink", "hardlink.db"))
            if self.hardlink_signature == snap.signature and self.hardlink:
                return self.hardlink
            index: dict[str, list[dict]] = {}
            try:
                rows = snap.con.execute(
                    "SELECT md5, type, file_name, dir1, dir2 FROM image_hardlink_info_v4"
                ).fetchall()
            except Exception:  # noqa: BLE001
                rows = []
            months: dict[int, str] = {}
            try:
                for rowid, name in snap.con.execute("SELECT _rowid_, username FROM dir2id").fetchall():
                    months[int(rowid)] = name or ""
            except Exception:  # noqa: BLE001
                try:
                    for rowid, name in snap.con.execute("SELECT rowid, username FROM dir2id").fetchall():
                        months[int(rowid)] = name or ""
                except Exception:  # noqa: BLE001
                    pass
            for md5_value, kind, file_name, dir1, dir2 in rows:
                if not md5_value or not file_name:
                    continue
                index.setdefault(md5_value.lower(), []).append(
                    {"type": kind, "file": file_name, "dir1": dir1, "month": months.get(int(dir2 or 0), "")}
                )
            self.hardlink = index
            self.hardlink_signature = snap.signature
            return index

    def find_dat(self, talker: str, md5_value: str):
        if re.fullmatch(r"[0-9a-fA-F]{32}", md5_value) is None:
            return
        entries = self.hardlink_index().get(md5_value.lower()) or []
        attach_root = Path(self.attach_root).resolve()
        talker_dir = (attach_root / md5_hex(talker)).resolve()
        if talker_dir != attach_root / md5_hex(talker):
            return
        for entry in entries:
            filename = entry["file"]
            if Path(filename).name != filename:
                continue
            thumb = filename.replace(".dat", "_t.dat")
            month = entry.get("month") or ""
            folders = [talker_dir / month / "Img", talker_dir] if month else [talker_dir]
            for folder in folders:
                for name in (filename, thumb):
                    candidate = (folder / name).resolve()
                    if candidate.is_relative_to(talker_dir) and candidate.is_file():
                        yield candidate

    def decrypt_dat(self, path: Path) -> bytes | None:
        try:
            with path.open("rb") as handle:
                data = handle.read(MAX_IMAGE_BYTES + 64)
        except OSError:
            return None
        if len(data) == MAX_IMAGE_BYTES + 64:
            return None
        signature = data[:6]
        xor_key = int(self.image_key.get("xorKey") or 0)
        if signature == b"\x07\x08V2\x08\x07":
            aes_key = str(self.image_key.get("aesKey") or "")
            if len(aes_key) >= 16:
                out = decrypt_v4(data, xor_key, aes_key[:16].encode("ascii"))
            else:
                out = None
        elif signature == b"\x07\x08V1\x08\x07":
            out = decrypt_v4(data, xor_key, b"cfcd208495d565ef")
        else:
            out = bytes(byte ^ xor_key for byte in data)
        return out if out and len(out) <= MAX_IMAGE_BYTES and is_image(out) else None

    def image_bytes(self, talker: str, md5_value: str) -> bytes | None:
        paths = self.find_dat(talker, md5_value)
        first = next(paths, None)
        if first is None:
            return None
        return self.decrypt_dat(first) or next(
            (data for path in paths if (data := self.decrypt_dat(path)) is not None),
            None,
        )


def quote_talker(talker: str) -> str:
    from urllib.parse import quote

    return quote(talker, safe="")


def unquote_talker(value: str) -> str:
    from urllib.parse import unquote

    return unquote(value)


def remove_padding(data: bytes) -> bytes:
    if not data:
        return data
    pad = data[-1]
    if 1 <= pad <= 16 and data[-pad:] == bytes([pad]) * pad:
        return data[:-pad]
    return data


def decrypt_v4(data: bytes, xor_key: int, aes_key: bytes) -> bytes | None:
    if len(data) < 0x0F:
        return None
    header = data[:0x0F]
    body = data[0x0F:]
    aes_size = int.from_bytes(header[6:10], "little")
    xor_size = int.from_bytes(header[10:14], "little")
    aligned = aes_size + (16 - aes_size % 16)
    if aligned > len(body):
        return None
    plain = remove_padding(AES.new(aes_key, AES.MODE_ECB).decrypt(body[:aligned]))
    rest = body[aligned:]
    if xor_size > 0:
        if xor_size > len(rest):
            return None
        raw = rest[: len(rest) - xor_size]
        tail = bytes(byte ^ xor_key for byte in rest[len(rest) - xor_size :])
    else:
        raw, tail = rest, b""
    return plain + raw + tail


def is_image(data: bytes) -> bool:
    return (
        data[:3] == b"\xff\xd8\xff"
        or data[:8] == b"\x89PNG\r\n\x1a\n"
        or data[:6] in (b"GIF87a", b"GIF89a")
        or (data[:4] == b"RIFF" and data[8:12] == b"WEBP")
    )


def load_config(path: str) -> dict:
    with open(path, "r", encoding="utf-8") as handle:
        config = json.load(handle)
    base = Path(path).resolve().parent
    config["cache_dir"] = str(base / config["cache_dir"])
    return config


def now() -> float:
    return time.time()
