# Completing and correcting tasks in Google Calendar

For an event linked to a dynamic (unpinned) Task, select an explicit event color
and set the start/end times to the work actually performed. Run `cargo run -- import`.
The Task becomes Done and a completion fact records those actual times.
Pinned commitments keep their existing behavior: time edits move their pin.

Import discovers events from 24 hours ago through one calendar month ahead by
default. The upper bound is the same UTC date/time next month, clamped to that
month's last day if needed. `--from` may extend the look-back but cannot reduce it
below 24 hours; `--to` may extend the look-ahead but cannot reduce it below one
calendar month. All result pages are fetched. Linked unfinished
dynamic Tasks missing from these results are also fetched by event ID, regardless
of date. Deleted events are ignored, and other fetch errors stop import before
the store is changed.

Events without both a start and end `dateTime` are ignored, including single-day
and multi-day date-only events. They do not create or update Tasks. Timed events
in the same response and later pages are still imported.

To undo a mistaken Calendar completion, restore the event's **Calendar default**
color and import again. The event's current end time must be less than 24 hours
ago (future end times also qualify). The cutoff uses the event time, not the time
the completion was imported. The Task returns to Backlog, its mistaken completion
is retracted from time reports, and it can be scheduled again. Completion facts
created by CLI `done`, which do not contain a Calendar actual window, are not
automatically undone by an uncolored event.

Duration edits to completed Tasks remain importable. Repeated imports do not add
duplicate completion facts; they can update the estimated duration. To replace
an already logged actual time window, undo and import first, then correct the
times, select an explicit color, and import again within the correction window.
Only the replacement completion counts in reports.

The import summary includes `completed`, `reopened`, and `resized` counts.

HTTP behavior follows Google's [events.list](https://developers.google.com/workspace/calendar/api/v3/reference/events/list)
and [events.get](https://developers.google.com/workspace/calendar/api/v3/reference/events/get)
APIs. Regression tests use a local HTTP server; live OAuth and Calendar UI
behavior still need operator verification.
