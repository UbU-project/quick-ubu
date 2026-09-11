from copy import deepcopy
from datetime import datetime, timedelta, timezone
import io
from pathlib import Path
import sqlite3
import unittest
from unittest.mock import patch

from tests.quick_ubu_test_copy import RESET_MARKER_TITLE, main, reset_and_copy


class Request:
    def __init__(self, value):
        self.value = value

    def execute(self):
        if isinstance(self.value, Exception):
            raise self.value
        return deepcopy(self.value)


class Calendar:
    def __init__(self, marker_pages=None, event_pages=None):
        self.marker_pages = marker_pages or [{"items": []}]
        self.event_pages = event_pages or [{"items": []}]
        self.calls = []
        self.inserts = []
        self.deletes = []

    def calendars(self):
        return self

    def events(self):
        return self

    def get(self, **kwargs):
        self.calls.append(("get", kwargs))
        return Request({"id": "dummy@example.test"})

    def list(self, **kwargs):
        self.calls.append(("list", kwargs))
        pages = self.marker_pages if "q" in kwargs else self.event_pages
        return Request(pages[int(kwargs["pageToken"] or 0)])

    def delete(self, **kwargs):
        self.calls.append(("delete", kwargs))
        self.deletes.append(kwargs["eventId"])
        return Request({})

    def insert(self, **kwargs):
        self.calls.append(("insert", kwargs))
        self.inserts.append(deepcopy(kwargs["body"]))
        return Request({"id": f"copy-{len(self.inserts)}"})


class ResetAndCopyTests(unittest.TestCase):
    def setUp(self):
        self.store = Path("memory/quick-ubu-store.db")
        # SQLite bytes and simulated paths stay in memory; no Google or disk I/O.
        connection = sqlite3.connect(":memory:")
        try:
            connection.execute("CREATE TABLE tasks (id TEXT PRIMARY KEY, data TEXT NOT NULL)")
            connection.execute("INSERT INTO tasks VALUES ('task-1', '{}')")
            connection.commit()
            self.original_store = connection.serialize()
        finally:
            connection.close()
        self.files = {self.store: self.original_store}
        self.sidecars = [Path(str(self.store) + suffix)
                         for suffix in ("-journal", "-wal", "-shm")]
        self.files.update({path: b"journal fixture" for path in self.sidecars})
        self.original_files = self.files.copy()

        def unlink(path, missing_ok=False):
            if path not in self.files and not missing_ok:
                raise FileNotFoundError(path)
            self.files.pop(path, None)

        self.unlink = unlink
        patcher = patch.object(Path, "unlink", unlink)
        patcher.start()
        self.addCleanup(patcher.stop)
        self.start = datetime(2026, 9, 5, tzinfo=timezone.utc)

    def marker(self, **extra):
        return {"id": "marker", "summary": RESET_MARKER_TITLE, **extra}

    def event(self, event_id, **extra):
        return {
            "id": event_id,
            "summary": "Task",
            "start": {"dateTime": (self.start + timedelta(hours=1)).isoformat()},
            "end": {"dateTime": (self.start + timedelta(hours=2)).isoformat()},
            **extra,
        }

    def assert_untouched(self, source, destination):
        self.assertEqual(self.files, self.original_files)
        self.assertEqual(source.deletes, [])
        self.assertEqual(source.inserts, [])
        self.assertEqual(destination.deletes, [])
        self.assertEqual(destination.inserts, [])

    def test_missing_or_inexact_marker_prevents_all_deletions(self):
        for title in [None, RESET_MARKER_TITLE.lower(), RESET_MARKER_TITLE + " ",
                      "prefix " + RESET_MARKER_TITLE, RESET_MARKER_TITLE + " suffix"]:
            with self.subTest(title=title):
                source = Calendar(marker_pages=[{"items": [self.marker()]}])
                destination = Calendar(
                    marker_pages=[{"items": [{"summary": title}]}],
                    event_pages=[{"items": [self.event("old")]}],
                )
                with self.assertRaises(ValueError) as error:
                    reset_and_copy(source, destination, self.store, self.start)
                self.assertIn(RESET_MARKER_TITLE, str(error.exception))
                self.assertIn("was not found", str(error.exception))
                self.assertIn("dummy@example.test", str(error.exception))
                self.assert_untouched(source, destination)
                self.assertEqual(source.calls, [])

    def test_cancelled_marker_does_not_authorize_reset(self):
        source = Calendar()
        destination = Calendar(marker_pages=[{"items": [self.marker(status="cancelled")]}])
        with self.assertRaises(ValueError):
            reset_and_copy(source, destination, self.store, self.start)
        self.assert_untouched(source, destination)

    def test_marker_lookup_failure_prevents_deletion(self):
        source = Calendar()
        destination = Calendar(marker_pages=[RuntimeError("lookup failed")])
        with self.assertRaisesRegex(RuntimeError, "lookup failed"):
            reset_and_copy(source, destination, self.store, self.start)
        self.assert_untouched(source, destination)

    def test_later_marker_page_failure_still_prevents_deletion(self):
        source = Calendar()
        destination = Calendar(marker_pages=[
            {"items": [{"summary": "Another event", "description": RESET_MARKER_TITLE}],
             "nextPageToken": "1"},
            RuntimeError("second marker page failed"),
        ])
        with self.assertRaisesRegex(RuntimeError, "second marker page failed"):
            reset_and_copy(source, destination, self.store, self.start)
        self.assert_untouched(source, destination)

    def test_exact_marker_on_later_page_outside_window_allows_all_colors(self):
        dynamic = self.event("dynamic", description="Notes", reminders={"useDefault": True})
        null_color = self.event("null", colorId=None)
        colored = self.event("fixed", colorId="3")
        empty_color = self.event("empty-color", colorId="")
        source = Calendar(event_pages=[
            {"items": [dynamic, colored], "nextPageToken": "1"},
            {"items": [null_color, empty_color]},
        ])
        destination = Calendar(
            marker_pages=[
                {"items": [], "nextPageToken": "1"},
                {"items": [self.marker(start={"date": "2000-01-01"})]},
            ],
            event_pages=[
                {"items": [self.event("old-dynamic")], "nextPageToken": "1"},
                {"items": [self.event("old-fixed", colorId="3")]},
            ],
        )
        report = reset_and_copy(source, destination, self.store, self.start)
        self.assertNotIn(self.store, self.files)
        self.assertTrue(all(path not in self.files for path in self.sidecars))
        self.assertEqual(destination.deletes, ["old-dynamic", "old-fixed"])
        self.assertEqual(destination.inserts, [
            {key: value for key, value in dynamic.items() if key != "id"},
            {key: value for key, value in colored.items() if key != "id"},
            {key: value for key, value in null_color.items() if key not in ("id", "colorId")},
            {key: value for key, value in empty_color.items() if key not in ("id", "colorId")},
        ])
        self.assertEqual(report["copied"], 4)
        self.assertEqual(report["deleted"], 2)
        self.assertEqual(report["events"], [
            {"source_id": "dynamic", "destination_id": "copy-1"},
            {"source_id": "fixed", "destination_id": "copy-2"},
            {"source_id": "null", "destination_id": "copy-3"},
            {"source_id": "empty-color", "destination_id": "copy-4"},
        ])
        for method, query in destination.calls:
            if method == "list" and "q" in query:
                self.assertNotIn("timeMin", query)
                self.assertNotIn("timeMax", query)
        for method, query in source.calls:
            self.assertEqual(method, "list")
            self.assertEqual(query["timeMin"], self.start.isoformat())
            self.assertEqual(query["timeMax"], (self.start + timedelta(days=14)).isoformat())

    def test_source_or_destination_listing_failure_preserves_store_and_calendar(self):
        for fail_source in [True, False]:
            with self.subTest(fail_source=fail_source):
                source = Calendar(event_pages=[RuntimeError("read failed")] if fail_source else None)
                destination = Calendar(
                    marker_pages=[{"items": [self.marker()]}],
                    event_pages=None if fail_source else [RuntimeError("read failed")],
                )
                with self.assertRaisesRegex(RuntimeError, "read failed"):
                    reset_and_copy(source, destination, self.store, self.start)
                self.assert_untouched(source, destination)

    def test_deletion_spans_yesterday_and_all_future_pages_independent_of_copy_start(self):
        now = self.start + timedelta(days=3)
        source = Calendar(event_pages=[{"items": [self.event("new")]}])
        destination = Calendar(
            marker_pages=[{"items": [self.marker()]}],
            event_pages=[
                {"items": [self.event(
                    "yesterday", start={"dateTime": (now - timedelta(hours=12)).isoformat()},
                    end={"dateTime": (now - timedelta(hours=11)).isoformat()},
                )], "nextPageToken": "1"},
                {"items": [self.event(
                    f"future-{days}", start={"dateTime": (now + timedelta(days=days)).isoformat()},
                    end={"dateTime": (now + timedelta(days=days, hours=1)).isoformat()},
                ) for days in [19, 365]]},
            ],
        )
        report = reset_and_copy(source, destination, self.store, self.start, now=now)
        deletion_queries = [query for method, query in destination.calls
                            if method == "list" and "q" not in query]
        self.assertEqual(len(deletion_queries), 2)
        for query in deletion_queries:
            self.assertEqual(query["timeMin"], (now - timedelta(hours=24)).isoformat())
            self.assertNotIn("timeMax", query)
        self.assertEqual(destination.deletes, ["yesterday", "future-19", "future-365"])
        self.assertEqual(report["delete_from"], deletion_queries[0]["timeMin"])
        self.assertIsNone(report["delete_to"])
        self.assertEqual(report["from"], self.start.isoformat())
        self.assertEqual(report["to"], (self.start + timedelta(days=14)).isoformat())
        for method, query in source.calls:
            self.assertEqual(method, "list")
            self.assertEqual(query["timeMin"], report["from"])
            self.assertEqual(query["timeMax"], report["to"])

    def test_only_starts_within_the_fourteen_day_window_are_copied(self):
        source = Calendar(event_pages=[{"items": [
            self.event("ongoing", start={"dateTime": (self.start - timedelta(hours=1)).isoformat()}),
            self.event("boundary", start={"dateTime": self.start.isoformat()}),
            self.event("too-late", start={"dateTime": (self.start + timedelta(days=14)).isoformat()}),
            self.event("cancelled", status="cancelled"),
        ]}])
        destination = Calendar(marker_pages=[{"items": [self.marker()]}])
        report = reset_and_copy(source, destination, self.store, self.start)
        self.assertEqual(report["events"], [{"source_id": "boundary", "destination_id": "copy-1"}])

    def test_all_day_events_keep_dates_and_colors_within_fourteen_day_window(self):
        events = [self.event(
            str(day),
            start={"date": (self.start + timedelta(days=day)).date().isoformat()},
            end={"date": (self.start + timedelta(days=day + 1)).date().isoformat()},
            **({"colorId": "5"} if day == 0 else {}),
        ) for day in [-1, 0, 13, 14]]
        source = Calendar(event_pages=[{"items": events}])
        destination = Calendar(marker_pages=[{"items": [self.marker()]}])
        report = reset_and_copy(
            source, destination, self.store, self.start + timedelta(hours=12),
        )
        self.assertEqual(report["copied"], 2)
        self.assertEqual(destination.inserts, [
            {key: value for key, value in event.items() if key != "id"}
            for event in events[1:3]
        ])

    def test_copy_preserves_content_and_omits_google_identity(self):
        original = self.event(
            "original-id",
            summary='Quotes " and newline\n',
            description="<b>Notes</b>",
            location="Home",
            transparency="transparent",
            visibility="private",
            reminders={"useDefault": False, "overrides": [{"method": "popup", "minutes": 10}]},
            status="confirmed",
            extendedProperties={"private": {"task": "example"}},
            source={"title": "Original", "url": "https://example.test/task"},
            colorId=None,
            etag="original-etag",
            iCalUID="original-uid",
            created="2026-01-01T00:00:00Z",
            updated="2026-01-01T00:00:00Z",
            htmlLink="https://example.test/original-event",
            creator={"email": "main@example.test"},
            organizer={"email": "main@example.test"},
        )
        source = Calendar(event_pages=[{"items": [original]}])
        destination = Calendar(marker_pages=[{"items": [self.marker()]}])
        reset_and_copy(source, destination, self.store, self.start)
        self.assertEqual(destination.inserts, [{
            key: original[key]
            for key in ["summary", "description", "location", "start", "end", "transparency",
                        "visibility", "reminders", "status", "extendedProperties", "source"]
        }])
        destination.inserts[0]["reminders"]["overrides"][0]["minutes"] = 99
        self.assertEqual(original["reminders"]["overrides"][0]["minutes"], 10)

    def test_empty_source_still_resets_destination_and_missing_store_is_allowed(self):
        self.store.unlink()
        source = Calendar()
        destination = Calendar(
            marker_pages=[{"items": [self.marker()]}],
            event_pages=[{"items": [self.event("old")]}],
        )
        report = reset_and_copy(source, destination, self.store, self.start)
        self.assertEqual(destination.deletes, ["old"])
        self.assertEqual(report["copied"], 0)
        self.assertNotIn(self.store, self.files)
        self.assertTrue(all(path not in self.files for path in self.sidecars))

    def test_store_deletion_failure_prevents_calendar_writes(self):
        source = Calendar()
        destination = Calendar(
            marker_pages=[{"items": [self.marker()]}],
            event_pages=[{"items": [self.event("old")]}],
        )
        with patch.object(Path, "unlink", side_effect=PermissionError("store is read-only")):
            with self.assertRaisesRegex(PermissionError, "store is read-only"):
                reset_and_copy(source, destination, self.store, self.start)
        self.assert_untouched(source, destination)

    def test_api_deletion_failure_aborts_copy_without_rolling_back_store(self):
        source = Calendar(event_pages=[{"items": [self.event("new")]}])
        destination = Calendar(
            marker_pages=[{"items": [self.marker()]}],
            event_pages=[{"items": [self.event("old")]}],
        )
        with patch.object(destination, "delete", return_value=Request(RuntimeError("delete failed"))):
            with self.assertRaisesRegex(RuntimeError, "delete failed"):
                reset_and_copy(source, destination, self.store, self.start)
        self.assertNotIn(self.store, self.files)
        self.assertTrue(all(path not in self.files for path in self.sidecars))
        self.assertEqual(destination.inserts, [])
        self.assertEqual(source.deletes, [])
        self.assertEqual(source.inserts, [])

    def test_reset_removes_only_selected_database_and_its_sidecars(self):
        unrelated = {
            Path("memory/quick-ubu-store.json"): b"legacy store",
            Path("memory/other.db"): b"other database",
            Path("memory/quick-ubu-store.wal"): b"unrelated suffix",
        }
        self.files.update(unrelated)
        source = Calendar()
        destination = Calendar(marker_pages=[{"items": [self.marker()]}])
        reset_and_copy(source, destination, self.store, self.start)
        self.assertEqual(self.files, unrelated)

    def test_absent_sidecars_are_allowed(self):
        for path in self.sidecars:
            self.files.pop(path)
        source = Calendar()
        destination = Calendar(marker_pages=[{"items": [self.marker()]}])
        reset_and_copy(source, destination, self.store, self.start)
        self.assertEqual(self.files, {})

    def test_sidecar_deletion_failure_prevents_calendar_writes(self):
        source = Calendar(event_pages=[{"items": [self.event("new")]}])
        destination = Calendar(
            marker_pages=[{"items": [self.marker()]}],
            event_pages=[{"items": [self.event("old")]}],
        )
        for failed_path in self.sidecars:
            with self.subTest(path=failed_path):
                self.files = self.original_files.copy()

                def unlink(path, missing_ok=False):
                    if path == failed_path:
                        raise PermissionError("journal is read-only")
                    self.unlink(path, missing_ok=missing_ok)

                with patch.object(Path, "unlink", unlink):
                    with self.assertRaisesRegex(PermissionError, "journal is read-only"):
                        reset_and_copy(source, destination, self.store, self.start)
                self.assertIn(failed_path, self.files)
                self.assertEqual(destination.deletes, [])
                self.assertEqual(destination.inserts, [])
                self.assertEqual(source.deletes, [])
                self.assertEqual(source.inserts, [])

    def test_cli_uses_sqlite_default_and_honors_custom_store(self):
        for arguments, expected in [
            ([], Path("quick-ubu-store.db")),
            (["--store", "custom/test.db"], Path("custom/test.db")),
        ]:
            with self.subTest(arguments=arguments):
                destination, source = Calendar(), Calendar()
                with patch("tests.quick_ubu_test_copy.calendar_service",
                           side_effect=[destination, source]), \
                        patch("tests.quick_ubu_test_copy.reset_and_copy", return_value={}) as reset, \
                        patch("sys.stdout", io.StringIO()):
                    result = main([
                        "--source-credentials", "main.json", "--source-token", "main.pickle",
                        "--destination-credentials", "dummy.json", "--destination-token", "dummy.pickle",
                        "--from", self.start.isoformat(), *arguments,
                    ])
                self.assertEqual(result, 0)
                self.assertEqual(reset.call_args.args, (source, destination, expected, self.start))

    def test_account_setup_error_identifies_role_and_token_file_before_reset(self):
        for role in ["destination", "source"]:
            with self.subTest(role=role):
                failure = RuntimeError("invalid_grant: Token has been expired or revoked.")
                effects = [failure] if role == "destination" else [Calendar(), failure]
                stderr, stdout = io.StringIO(), io.StringIO()
                with patch("tests.quick_ubu_test_copy.calendar_service", side_effect=effects), \
                        patch("tests.quick_ubu_test_copy.reset_and_copy") as reset, \
                        patch("sys.stderr", stderr), patch("sys.stdout", stdout):
                    result = main([
                        "--source-credentials", "main-credentials.json", "--source-token", "main-token.pickle",
                        "--destination-credentials", "dummy-credentials.json", "--destination-token", "dummy-token.pickle",
                        "--store", str(self.store), "--from", self.start.isoformat(),
                    ])
                prefix = "dummy" if role == "destination" else "main"
                message = stderr.getvalue()
                self.assertEqual(result, 1)
                self.assertIn(f"{role} account setup failed", message)
                self.assertIn(f'token file "{prefix}-token.pickle"', message)
                self.assertIn(f'credentials file "{prefix}-credentials.json"', message)
                self.assertIn("invalid_grant: Token has been expired or revoked.", message)
                self.assertEqual(stdout.getvalue(), "")
                reset.assert_not_called()
                self.assertEqual(self.files, self.original_files)

    def test_cli_returns_error_for_missing_destination_marker(self):
        destination = Calendar()
        source = Calendar()
        stderr = io.StringIO()
        with patch("tests.quick_ubu_test_copy.calendar_service", side_effect=[destination, source]), \
                patch("sys.stderr", stderr):
            result = main([
                "--source-credentials", "main-credentials.json", "--source-token", "main-token.pickle",
                "--destination-credentials", "dummy-credentials.json", "--destination-token", "dummy-token.pickle",
                "--store", str(self.store), "--from", self.start.isoformat(),
            ])
        self.assertEqual(result, 1)
        self.assertIn(f'An Event with the exact title "{RESET_MARKER_TITLE}" was not found', stderr.getvalue())
        self.assert_untouched(source, destination)


if __name__ == "__main__":
    unittest.main()
