from copy import deepcopy
from datetime import datetime, timedelta, timezone
import io
from pathlib import Path
import tempfile
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
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.store = Path(self.directory.name) / "quick-ubu-store.json"
        self.store.write_text("original store")
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
        self.assertEqual(self.store.read_text(), "original store")
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

    def test_exact_marker_on_later_page_outside_window_allows_dynamic_copy_only(self):
        dynamic = self.event("dynamic", description="Notes", reminders={"useDefault": True})
        null_color = self.event("null", colorId=None)
        source = Calendar(event_pages=[
            {"items": [dynamic, self.event("fixed", colorId="3")], "nextPageToken": "1"},
            {"items": [null_color, self.event("empty-color", colorId="")]},
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
        self.assertFalse(self.store.exists())
        self.assertEqual(destination.deletes, ["old-dynamic", "old-fixed"])
        self.assertEqual(destination.inserts, [
            {key: value for key, value in event.items() if key not in ("id", "colorId")}
            for event in [dynamic, null_color]
        ])
        self.assertEqual(report["copied"], 2)
        self.assertEqual(report["deleted"], 2)
        self.assertEqual(report["events"], [
            {"source_id": "dynamic", "destination_id": "copy-1"},
            {"source_id": "null", "destination_id": "copy-2"},
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

    def test_deletion_spans_yesterday_to_fourteen_days_from_now_independent_of_copy_start(self):
        now = self.start + timedelta(days=3)
        source = Calendar(event_pages=[{"items": [self.event("new")]}])
        destination = Calendar(
            marker_pages=[{"items": [self.marker()]}],
            event_pages=[{"items": [self.event(
                "yesterday", start={"dateTime": (now - timedelta(hours=12)).isoformat()},
                end={"dateTime": (now - timedelta(hours=11)).isoformat()},
            )]}],
        )
        report = reset_and_copy(source, destination, self.store, self.start, now=now)
        deletion_queries = [query for method, query in destination.calls
                            if method == "list" and "q" not in query]
        self.assertEqual(len(deletion_queries), 1)
        self.assertEqual(deletion_queries[0]["timeMin"], (now - timedelta(hours=24)).isoformat())
        self.assertEqual(deletion_queries[0]["timeMax"], (now + timedelta(days=14)).isoformat())
        self.assertEqual(destination.deletes, ["yesterday"])
        self.assertEqual(report["delete_from"], deletion_queries[0]["timeMin"])
        self.assertEqual(report["delete_to"], deletion_queries[0]["timeMax"])
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
        self.assertFalse(self.store.exists())

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
        self.assertFalse(self.store.exists())
        self.assertEqual(destination.inserts, [])
        self.assertEqual(source.deletes, [])
        self.assertEqual(source.inserts, [])

    def test_cli_returns_error_for_missing_destination_marker(self):
        destination = Calendar()
        source = Calendar()
        stderr = io.StringIO()
        with patch("quick_ubu_test_copy.calendar_service", side_effect=[destination, source]), \
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
