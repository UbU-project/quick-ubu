# Copy dynamic events to the Quick UbU dummy account

`quick_ubu_test_copy.py` resets the dummy primary calendar from **24 hours before
the current time through 14 days after the current time**, removes the specified
local store, and copies main-account dynamic events over a separate 14-day
interval starting at `--from` (current time by default). Only `event.get("colorId") is None`
qualifies for copying. The source calendar is only read.

Only the two accounts' primary calendars are used; the Google account itself
and other calendars are not deleted. Source events must start at or after
`--from` and before the end of the 14-day copy interval. Destination deletion uses
Google's overlap filter (`end > now - 24 hours` and `start < now + 14 days`), so an
event already in progress at the deletion cutoff is also deleted. Events outside
that overlap are kept. The current time is captured once when the command starts;
`--from` changes only copying and does not move the deletion window.
See Google's [events.list parameters](https://developers.google.com/workspace/calendar/api/v3/reference/events/list).

Before either calendar events or the local store can be deleted, the dummy
primary calendar must contain a non-cancelled event whose title is exactly:

```text
DELETE THIS ACCOUNT FOR QUICK UBU TEST
```

The comparison is case-sensitive and does not trim whitespace. The marker can
be outside the test interval; the lookup follows all search-result pages. If
it is missing, the script exits with an error naming the destination account
and the required title, without deleting anything. A lookup error also aborts.
If the marker falls within the reset interval, it is deleted along with the
other destination events and must be recreated before the next reset. Keeping
it in the past with an end time more than 24 hours before the current time avoids that.

Use the Python environment that runs `workhoursquery.py` (with
`google-api-python-client`, `google-auth`, and `google-auth-oauthlib` installed):

```sh
python quick_ubu_test_copy.py \
  --source-credentials /path/to/main/credentials.json \
  --source-token /path/to/main/token.pickle \
  --destination-credentials /path/to/dummy/credentials.json \
  --destination-token /path/to/dummy/token.pickle \
  --store quick-ubu-store.json
```

Both token pickle files must already exist. A valid token supplies the account
identity; its paired `credentials.json` is used only if interactive authorization
is needed. Expired tokens with refresh tokens are refreshed, and refreshed or
newly authorized credentials are written back to that account's pickle file.
Thus source calendar events are read-only, but its local token file can change.
All relative paths, including the default store path, resolve from the current
working directory.

The marker authorizes the reset; there is no additional confirmation prompt.
The default copy interval starts at the current UTC time. Use
`--from 2026-09-05T00:00:00-04:00` to choose a fixed copy start. The JSON output gives
the copy interval as `from`/`to`, the deletion interval as `delete_from`/`delete_to`,
deletion/copy counts, and source-to-destination event IDs.

Both event lists are fetched completely before deletion begins. Calendar writes
are sequential and are not transactional: an API failure during deletion or
copying can leave a partial reset. This utility assumes the agreed simple,
timed, non-recurring dynamic-event pattern. It copies the supported content
fields listed in `COPY_FIELDS`, omits original identities and color, and does
not copy attendees or conference data. All destination events returned for the
deletion interval are deleted, including colored events.

The store is removed before the first calendar deletion; a failure removing it
prevents calendar writes. No backup or rollback is performed. A successful
source read with zero qualifying tasks still resets the destination deletion interval.
Reminder settings are copied as data: `useDefault: true` selects the destination
calendar's defaults, which may differ from the source account's defaults.

After copying, use Quick UbU's dummy-account credentials and its separate OAuth
token cache to import the same interval. Re-import `routine.json` before
`generate`, since resetting the store removes routine templates and settings.

For example, if the copy starts at `2026-09-05T00:00:00-04:00`, use:

```sh
cargo run -- import \
  --credentials /path/to/dummy/credentials.json \
  --token-cache /path/to/dummy/token-cache.json \
  --from 2026-09-05T00:00:00-04:00 --to 2026-09-19T00:00:00-04:00
cargo run -- routine-import routine.json
cargo run -- generate --from 2026-09-05 --days 14 --tz America/New_York
```

Use the copy report's actual timestamps for import; import and generate default
to seven days when their horizon options are omitted. Generation uses local
calendar dates, so a copy starting mid-day will not exactly match its boundaries.
For a precise comparison, use a midnight `--from` and a corresponding timezone.
The copy interval is 14 times 24 hours with a fixed UTC offset; if it crosses a
daylight-saving transition, check the final boundary against the generated local
dates as well.
If the copy uses a non-default `--store`, pass that path to each Quick UbU command.
`token-cache.json` above is Quick UbU's OAuth cache, not the Python pickle file.

Offline verification (no Google dependencies or HTTP needed):

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s tests -v
```
