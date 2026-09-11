#!/usr/bin/env python3
"""Reset the dummy calendar and copy 14 days of Google Calendar events, in all colors."""

import argparse
from copy import deepcopy
from datetime import date, datetime, timedelta, timezone
import json
from pathlib import Path
import pickle
import sys


RESET_MARKER_TITLE = "DELETE THIS ACCOUNT FOR QUICK UBU TEST"
CALENDAR_ID = "primary"
SCOPES = ["https://www.googleapis.com/auth/calendar"]
COPY_FIELDS = (
    "summary", "description", "location", "start", "end", "transparency",
    "visibility", "reminders", "status", "extendedProperties", "source",
)


def calendar_service(credentials_path, token_path):
    # Keep Google dependencies optional for --help and offline unit tests.
    from google.auth.transport.requests import Request
    from google_auth_oauthlib.flow import InstalledAppFlow
    from googleapiclient.discovery import build

    with token_path.open("rb") as token_file:
        credentials = pickle.load(token_file)
    if not credentials or not credentials.valid:
        if credentials and credentials.expired and credentials.refresh_token:
            credentials.refresh(Request())
        else:
            flow = InstalledAppFlow.from_client_secrets_file(str(credentials_path), SCOPES)
            credentials = flow.run_local_server(port=0)
        with token_path.open("wb") as token_file:
            pickle.dump(credentials, token_file)
    return build("calendar", "v3", credentials=credentials, cache_discovery=False)


def account_calendar_service(account, credentials_path, token_path):
    """Identify authentication/setup failures without printing credential contents."""
    try:
        return calendar_service(credentials_path, token_path)
    except Exception as error:
        raise RuntimeError(
            f'{account} account setup failed (token file "{token_path}", '
            f'credentials file "{credentials_path}"): {error}'
        ) from error


def list_events(service, **query):
    page_token = None
    while True:
        page = service.events().list(
            calendarId=CALENDAR_ID,
            maxResults=2500,
            showDeleted=False,
            pageToken=page_token,
            **query,
        ).execute()
        yield from page.get("items", [])
        page_token = page.get("nextPageToken")
        if not page_token:
            return


def require_destination_marker(destination):
    destination_id = destination.calendars().get(calendarId=CALENDAR_ID).execute()["id"]
    # q is a search aid, not an exact-title comparison. Deliberately omit date
    # bounds so the marker can live outside the 14-day test window.
    for event in list_events(destination, q=RESET_MARKER_TITLE, singleEvents=False):
        if (
            event.get("summary") == RESET_MARKER_TITLE
            and event.get("status") != "cancelled"
        ):
            return destination_id
    raise ValueError(
        f'An Event with the exact title "{RESET_MARKER_TITLE}" was not found '
        f'for destination account "{destination_id}". '
        "No calendar events or local store were deleted."
    )


def reset_and_copy(source, destination, store_path, start, *, now=None):
    """The marker check must precede every destructive operation in this flow."""
    destination_id = require_destination_marker(destination)
    if start.tzinfo is None or start.utcoffset() is None:
        raise ValueError("The start timestamp must include a timezone offset.")
    if now is None:
        now = datetime.now(timezone.utc)
    delete_start = now - timedelta(hours=24)
    end = start + timedelta(days=14)
    query = {
        "timeMin": start.isoformat(),
        "timeMax": end.isoformat(),
        "singleEvents": True,
        "orderBy": "startTime",
    }

    # Fetch both complete lists before any deletion; a failed read is harmless.
    # timeMin tests event end times, so check the source start time explicitly.
    copies = []
    for event in list_events(source, **query):
        if event.get("status") == "cancelled":
            continue
        if "dateTime" in event["start"]:
            event_start = datetime.fromisoformat(event["start"]["dateTime"].replace("Z", "+00:00"))
            in_window = start <= event_start < end
        else:
            # All-day events use dates, not timestamps. Include the 14 calendar
            # dates beginning on the requested start date.
            event_date = date.fromisoformat(event["start"]["date"])
            in_window = start.date() <= event_date < end.date()
        if not in_window:
            continue
        body = {key: deepcopy(event[key]) for key in COPY_FIELDS if key in event}
        # Preserve explicit event colors; omit unset colors to keep the default.
        if event.get("colorId"):
            body["colorId"] = event["colorId"]
        copies.append((event["id"], body))
    deletion_query = {
        "timeMin": delete_start.isoformat(),
        # No timeMax: remove future copies regardless of how far out they moved.
        "singleEvents": True,
        "orderBy": "startTime",
    }
    deletions = list(list_events(destination, **deletion_query))

    # Reset the whole SQLite store, including journal files left by an interrupted
    # process. Quick UbU must be stopped while its database is being removed.
    # Append suffixes: SQLite uses store.db-wal, not store.wal.
    for path in [store_path, *(Path(str(store_path) + suffix)
                               for suffix in ("-journal", "-wal", "-shm"))]:
        path.unlink(missing_ok=True)
    for event in deletions:
        destination.events().delete(
            calendarId=CALENDAR_ID, eventId=event["id"], sendUpdates="none",
        ).execute()
    mapping = []
    for source_id, body in copies:
        created = destination.events().insert(
            calendarId=CALENDAR_ID, body=body, sendUpdates="none",
        ).execute()
        mapping.append({"source_id": source_id, "destination_id": created["id"]})
    return {
        "destination_account": destination_id,
        "from": start.isoformat(),
        "to": end.isoformat(),
        "delete_from": delete_start.isoformat(),
        "delete_to": None,  # Unbounded future deletion window.
        "deleted": len(deletions),
        "copied": len(mapping),
        "events": mapping,
    }


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-credentials", type=Path, required=True)
    parser.add_argument("--source-token", type=Path, required=True)
    parser.add_argument("--destination-credentials", type=Path, required=True)
    parser.add_argument("--destination-token", type=Path, required=True)
    parser.add_argument(
        "--store", type=Path, default=Path("quick-ubu-store.db"),
        help="SQLite store to remove, including journals (default: quick-ubu-store.db); stop Quick UbU before resetting",
    )
    parser.add_argument("--from", dest="start", help="Copy start ISO timestamp with timezone; defaults to now; deletion covers 24 hours ago onward with no future limit")
    args = parser.parse_args(argv)
    try:
        now = datetime.now(timezone.utc)
        start = (
            datetime.fromisoformat(args.start.replace("Z", "+00:00"))
            if args.start else now
        )
        destination = account_calendar_service(
            "destination", args.destination_credentials, args.destination_token,
        )
        source = account_calendar_service(
            "source", args.source_credentials, args.source_token,
        )
        report = reset_and_copy(source, destination, args.store, start, now=now)
    except Exception as error:
        print(f"quick-ubu-test-copy: {error}", file=sys.stderr)
        return 1
    print(json.dumps(report, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main())
