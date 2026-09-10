# Schedule-weighted review order

`review` and `prioritize` present pending decisions in a random order strongly
biased toward dynamic Tasks scheduled soon. Each session computes a fresh
deterministic Plan using the same defaults as `export` (start now, affect cap
100). The order is fixed for that session; the next invocation uses the updated
Tasks and relationships.

A comparison receives the larger weight of its two Tasks, so a near-term Task
is still emphasized when compared with a much later Task. Preference and
dependency decisions both use this rule.

For a scheduled dynamic Task, the weight is
`1 + 63 / (1 + days_until_start)^2`:

| Planned start | Relative weight |
| --- | ---: |
| Now | 64 |
| In 24 hours | 16.75 |
| In 7 days | 1.98 |
| No upcoming scheduled entry | 1 |

These are selection weights, not guarantees of a particular position. Every
pending decision appears once if the session is completed. Pinned Tasks,
inactive Tasks, and entries that have already ended receive no schedule boost.
For multiple upcoming entries of the same Task, the earliest start determines
its weight.

If planning fails, the command reports the failure and uses the uniform random
order so review remains available to repair relationships. Presentation does
not reorder the stored pending queue.
