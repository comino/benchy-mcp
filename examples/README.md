# Example bench

```sh
./target/release/benchy-mcp ./examples/data     # or, from a release bundle, benchy-mcp ./example-data
```

Started by hand it only waits for an MCP client on stdin; register it with your client instead,
by absolute path (see the README's Quickstart).

Two tables, `meters` and `rooms`, and a deliberate trap.

`meters` covers one hour at 15-minute intervals. **The lab is complete; the office is not** — its
00:30 reading is missing and its 00:15 reading appears twice. An agent that sums `power_kw` and
multiplies by 0.25 gets 5.0 kWh for the lab, which is right, and 1.0 kWh for the office, which is
*arithmetically* right and *analytically* wrong: the hour has a hole in it and a duplicate, and
the true consumption is unknown.

That is the point. A correct sum over a hole in the data looks exactly like a correct answer
from the outside. The bench computes it all the same, and `describe` says what is wrong with the
data before the agent sums it: `office` has a gap, a row missing and a row that repeats a time.

`rooms` has a `plant` row with no matching meter, so a naive inner join silently drops it.

Things worth asking an agent to try:

- *"How much energy did each meter use, and which answer should I not trust?"*
- *"Is any meter missing readings? Show me how you know."*
- *"Join rooms to meters and tell me what you lost."*

## With a config and helpers

```sh
./target/release/benchy-mcp --config ./examples/benchy.toml
```

[`benchy.toml`](benchy.toml) serves the same two files, the [`connectors/hello.sh`](connectors/hello.sh)
connector as a third table, `readings`, and the SQL helpers in [`helpers.sql`](helpers.sql): an
`hourly` view and the `dewpoint` and `hours_above` macros, which `describe` lists. The config
documents every key inline.

## A host

```sh
python3 examples/host.py ./target/release/benchy-mcp
node examples/host-node.mjs ./target/release/benchy-mcp
```

The same bench without an MCP client in front: [`host.py`](host.py) drives `benchy-mcp host` over
its framed protocol ([docs/HOST-PROTOCOL.md](../docs/HOST-PROTOCOL.md)), attaches
[`connectors/hello.arrows`](connectors/hello.arrows), and queries it the way an agent would.
[`host-node.mjs`](host-node.mjs) is a host on Node.js, which cannot pass a descriptor, and so
streams its tables through the session's stream channel.
