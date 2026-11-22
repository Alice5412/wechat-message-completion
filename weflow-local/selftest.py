"""Run repeatable tests with synthetic databases and attachments."""

import http.client
import importlib.util
import json
import sqlite3
import threading
import unittest
from http.server import HTTPServer
from pathlib import Path
from tempfile import TemporaryDirectory
from unittest.mock import Mock, patch

import wfcore


class ConfigTests(unittest.TestCase):
    def test_relative_cache_paths_follow_the_config(self):
        with TemporaryDirectory() as directory:
            path = Path(directory, "config.json")
            path.write_text('{"cache_dir": ".cache"}', encoding="utf-8")
            config = wfcore.load_config(str(path))
            self.assertEqual(Path(config["cache_dir"]), path.resolve().parent / ".cache")


class SnapshotTests(unittest.TestCase):
    def setUp(self):
        self.directory = self.enterContext(TemporaryDirectory())
        self.source = Path(self.directory, "source.db")
        self.cache = Path(self.directory, "cache")
        self.cache.mkdir()
        self.db = sqlite3.connect(self.source)
        self.addCleanup(self.db.close)
        self.db.execute("PRAGMA journal_mode = WAL")
        self.db.execute("PRAGMA wal_autocheckpoint = 0")
        self.db.execute("CREATE TABLE messages (body TEXT)")
        self.append("first")
        self.db.execute("PRAGMA wal_checkpoint(TRUNCATE)")
        self.enterContext(patch.object(wfcore.sqlcipher3, "connect", sqlite3.connect))

    def append(self, text):
        self.db.execute("INSERT INTO messages VALUES (?)", (text,))
        self.db.commit()

    def snapshot(self):
        keys = Mock()
        keys.raw_key.return_value = bytes(32)
        snapshot = wfcore.Snapshot(str(self.source), str(self.cache), keys)
        self.addCleanup(snapshot.close)
        return snapshot

    def test_allocates_fresh_paths_and_does_not_copy_shm(self):
        self.append("newest")
        first = self.snapshot()
        second = self.snapshot()
        with patch.object(wfcore.shutil, "copyfile", wraps=wfcore.shutil.copyfile) as copy:
            first.refresh()
            second.refresh()
        self.assertIsNone(first.error)
        self.assertIsNone(second.error)
        self.assertNotEqual(first.directory.name, second.directory.name)
        self.assertEqual(first.con.execute("SELECT body FROM messages").fetchall(),
                         [("first",), ("newest",)])
        self.assertEqual([Path(call.args[0]).name for call in copy.call_args_list],
                         ["source.db", "source.db-wal", "source.db", "source.db-wal"])

    def test_retries_if_wal_changes_during_copy(self):
        self.append("second")
        snapshot = self.snapshot()
        copyfile = wfcore.shutil.copyfile
        source_copies = 0

        def copy_while_writing(source, target):
            nonlocal source_copies
            result = copyfile(source, target)
            if Path(source) == self.source:
                source_copies += 1
                if source_copies == 1:
                    self.append("third")
            return result

        with patch.object(wfcore.shutil, "copyfile", side_effect=copy_while_writing):
            snapshot.refresh()
        self.assertIsNone(snapshot.error)
        self.assertEqual(source_copies, 2)
        self.assertEqual(snapshot.con.execute("SELECT body FROM messages").fetchall(),
                         [("first",), ("second",), ("third",)])


class SessionTests(unittest.TestCase):
    def test_same_name_in_contacts_blocks_an_existing_session(self):
        store = object.__new__(wfcore.Store)
        store.lock = threading.RLock()
        store.contacts_signature = 1
        store.contacts = {
            "alice": {"name": "讨论", "nick": "", "alias": ""},
            "bob": {"name": "其他", "nick": "讨 论", "alias": ""},
        }
        store.db_root = "fixture"
        snapshot = Mock(signature=1)
        snapshot.con.execute.return_value.fetchall.return_value = [
            ("alice", "", 1, 0, 0)
        ]
        store.snapshot = Mock(return_value=snapshot)
        with self.assertRaises(wfcore.AmbiguousTitle):
            store.sessions(title="讨论")
        self.assertEqual(store.sessions(title="alice")[0]["username"], "alice")


class MediaTests(unittest.TestCase):
    def test_thumbnail_lookup_is_scoped_to_conversation(self):
        image = b"\x89PNG\r\n\x1a\nfixture"
        media_id = "a" * 32
        with TemporaryDirectory() as directory:
            attach = Path(directory, "attach")
            files = attach / wfcore.md5_hex("alice") / "2026-11" / "Img"
            files.mkdir(parents=True)
            (files / "fixture.dat").write_bytes(b"invalid")
            (files / "fixture_t.dat").write_bytes(image)
            store = object.__new__(wfcore.Store)
            store.attach_root = str(attach)
            store.image_key = {}
            store.hardlink_index = Mock(
                return_value={media_id: [{"file": "fixture.dat", "month": "2026-11"}]}
            )
            self.assertEqual(store.image_bytes("alice", media_id.upper()), image)
            self.assertIsNone(store.image_bytes("bob", media_id))


class HandlerTests(unittest.TestCase):
    def test_authentication_protocol_and_media_identifier(self):
        store = Mock(root="test-data")
        store.sessions.side_effect = wfcore.AmbiguousTitle
        store.image_bytes.return_value = None
        spec = importlib.util.spec_from_file_location(
            "weflow_test_server", Path(__file__).with_name("server.py")
        )
        module = importlib.util.module_from_spec(spec)
        with (
            patch.object(wfcore, "load_config", return_value={"token": "test-token", "wxid": "fixture"}),
            patch.object(wfcore, "Store", return_value=store),
            patch("os.makedirs"),
        ):
            spec.loader.exec_module(module)
        with patch.object(module, "log"), HTTPServer(("127.0.0.1", 0), module.Handler) as server:
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                with self.connection(server.server_port) as http:
                    self.assertEqual(self.request(http, "/api/v1/health", {}), (401, None))
                    status, health = self.request(http, "/api/v1/health")
                    self.assertEqual(status, 200)
                    self.assertEqual(health["version"], 2)
                    self.assertEqual(health["dataRoot"], "test-data")
                    self.assertEqual(self.request(http, "/api/v1/sessions?title=fixture")[0], 409)
                    self.assertEqual(self.request(http, "/api/v1/media/alice/invalid.img")[0], 400)
                    store.image_bytes.assert_not_called()
                    self.assertEqual(self.request(http, f"/api/v1/media/alice/{'A' * 32}.img")[0], 404)
                    store.image_bytes.assert_called_once_with("alice", "a" * 32)
            finally:
                server.shutdown()
                thread.join()

    @staticmethod
    def request(http, route, headers=None):
        if headers is None:
            headers = {"Authorization": "Bearer test-token"}
        http.request("GET", route, headers=headers)
        response = http.getresponse()
        body = response.read()
        return response.status, json.loads(body) if response.status == 200 else None

    @staticmethod
    def connection(port):
        from contextlib import closing

        return closing(http.client.HTTPConnection("127.0.0.1", port, timeout=3))


if __name__ == "__main__":
    unittest.main()
