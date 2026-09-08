# Ollama streaming and timeouts

`advise` and `replan --planner ollama` use Ollama's streaming `/api/generate`
endpoint. Quick UbU assembles the response fragments and only passes the answer
to the advisor or planner after Ollama reports `done: true`. Thinking remains
disabled (`think: false`); any thinking text returned by the model is excluded
from the answer.

Two deadlines apply, both measured from request start:

- `--ollama-timeout` (default **300 seconds**) limits the wait for the first
  complete stream message. This includes connecting, model loading, and waiting
  for that message. HTTP headers, blank lines, and incomplete message bytes do
  not satisfy or reset this deadline. A complete thinking message does count as
  a response, even when its answer fragment is empty.
- `--ollama-total-timeout` (default **600 seconds**) limits the entire request,
  including startup. Stream progress does not reset this deadline. It also
  applies during startup if configured shorter than the first-response limit.

For example:

```sh
cargo run -- advise --ollama-timeout 300 --ollama-total-timeout 600
cargo run -- replan --planner ollama --ollama-timeout 300 --ollama-total-timeout 600
```

After the first message, a generation can run past 300 seconds, including long
pauses between messages, provided it completes before the total deadline.
Streaming cannot shorten model loading or the wait for the first message. If a
slow GPU requires longer, increase the relevant limits.

On timeout, Quick UbU cancels its pending HTTP operation and discards the partial
answer. `advise` reports which deadline expired; the Ollama planner retains its
existing deterministic fallback on errors. A stream that ends without
`done: true` also fails, even if the partial answer looks like valid JSON.
Empty completed answers retain completion diagnostics, including whether any
thinking was present, without printing the thinking text.

Protocol reference: [Ollama streaming documentation](https://docs.ollama.com/api/streaming).
