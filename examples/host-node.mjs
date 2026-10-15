#!/usr/bin/env node
// A host on Node.js that streams its tables: no file on either side, and no descriptor passing.
//
//     node examples/host-node.mjs ./target/release/benchy-mcp
//
// Node.js has no public way to send a descriptor (SCM_RIGHTS) inside benchy's framing, so the
// streamed attach that passes a pipe with the request (`"stream": true`) is out of its reach.
// The stream channel needs none: benchy is started with one more pipe, `stdio[3]`, named by
// `--stream-channel 3`, and the host writes Arrow streams into it back to back, as it fetches
// them; each `attach` with `"stream": "channel"` takes the next whole one (protocol 5,
// docs/HOST-PROTOCOL.md). Here the "fetch" is the bundled Arrow stream, read from disk.
//
// Standard library only. Exits non-zero if the protocol does not do what the document says.
import { spawn } from "node:child_process";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const binary = process.argv[2];
if (!binary) {
  console.error("usage: node examples/host-node.mjs ./target/release/benchy-mcp");
  process.exit(2);
}
const here = dirname(fileURLToPath(import.meta.url));
const platform = process.platform === "linux" ? [] : ["--allow-unsandboxed"];

// stdin and stdout carry the protocol, stderr is benchy's log, and stdio[3] is the channel.
const benchy = spawn(binary, ["host", "--stream-channel", "3", ...platform], {
  stdio: ["pipe", "pipe", "inherit", "pipe"],
});
const channel = benchy.stdio[3];
// Once benchy closes the channel (a stream it lost its place in, or a refused request that named
// it), writing into it fails: on Node with ECONNRESET, not EPIPE, and a stream with no 'error'
// handler would take the whole host down. Node may destroy the stream as soon as the read end
// goes, before the next write, so the host keeps the error and attaches by another form.
let channelError = null;
channel.on("error", (e) => {
  channelError ??= e;
});

// Replies: four bytes of big-endian length, then one JSON object.
let buffered = Buffer.alloc(0);
const waiting = [];
benchy.stdout.on("data", (chunk) => {
  buffered = Buffer.concat([buffered, chunk]);
  while (buffered.length >= 4) {
    const n = buffered.readUInt32BE(0);
    if (buffered.length < 4 + n) break;
    const reply = JSON.parse(buffered.subarray(4, 4 + n).toString("utf8"));
    buffered = buffered.subarray(4 + n);
    waiting.shift()(reply);
  }
});

let next = 0;
function call(name, fields = {}) {
  next += 1;
  const body = Buffer.from(JSON.stringify({ id: next, call: name, ...fields }));
  const len = Buffer.alloc(4);
  len.writeUInt32BE(body.length);
  benchy.stdin.write(Buffer.concat([len, body]));
  return new Promise((resolve) => waiting.push(resolve));
}

const failures = [];
function check(what, ok, detail) {
  console.log(`  ${ok ? "ok  " : "FAIL"} ${what}${ok ? "" : `  — ${JSON.stringify(detail)}`}`);
  if (!ok) failures.push(what);
}

const hello = await call("hello", { protocol: [1, 2, 3, 4, 5] });
check("hello agrees protocol 5, which has the stream channel", hello.protocol === 5, hello);

// A fetch, as it would come off the network: written into the channel as it arrives. The
// streams of two tables go back to back; benchy reads each to its end-of-stream marker.
const stream = readFileSync(join(here, "connectors", "hello.arrows"));
for (let i = 0; i < stream.length; i += 1024) channel.write(stream.subarray(i, i + 1024));
channel.write(stream);

let r = await call("attach", { name: "readings", stream: "channel" });
check("an attach takes the first stream from the channel", r.ok === true, r);
console.log(`       readings: ${r.rows} rows`);
r = await call("attach", { name: "again", stream: "channel" });
check("the next attach takes the next stream", r.ok === true && r.rows > 0, r);

// A refused table's stream is read on to its marker and dropped, so the next attach still takes
// the next stream: here a name the agent could not type.
channel.write(stream);
channel.write(stream);
r = await call("attach", { name: "Bad", stream: "channel" });
check("a refused table is refused", r.ok === false && r.error.code === "E_NAME", r);
r = await call("attach", { name: "again", stream: "channel" });
check("and the channel is still at the next stream", r.ok === true && r.rows > 0, r);

r = await call("query", { sql: "SELECT count(*) AS n FROM readings" });
check("the agent's SQL runs over what was streamed", r.ok === true, r);
r = await call("describe");
check(
  "describe lists both tables",
  JSON.stringify(r.tables.map((t) => t.name)) === JSON.stringify(["readings", "again"]),
  r,
);

// A request refused whole that names the channel closes it, whether or not its streams were
// written ahead: the next attach must not take one of them under another name.
channel.write(stream);
r = await call("attach", {
  tables: [
    { name: "meters", stream: "channel" },
    { name: "meters", stream: "channel" },
  ],
});
check(
  "a refused request that names the channel closes it",
  r.ok === false && r.error.code === "E_REQUEST" && r.error.channel === "closed",
  r,
);
r = await call("attach", { name: "rooms", stream: "channel" });
check("and no later attach takes its stream", r.ok === false && r.error.code === "E_CHANNEL", r);
await new Promise((resolve) => channel.write(stream, resolve));
check(
  "writing into the closed channel fails, and the host stays up",
  ["ECONNRESET", "EPIPE"].includes(channelError?.code),
  channelError?.code,
);
r = await call("query", { sql: "SELECT count(*) AS n FROM readings" });
check("the session goes on", r.ok === true, r);

channel.end();
benchy.stdin.end(); // the session ends when its stdin does
const code = await new Promise((resolve) => benchy.on("exit", resolve));
check("the session ends when stdin closes", code === 0, code);
console.log(failures.length ? `\n${failures.length} check(s) failed` : "\nall checks passed");
process.exit(failures.length ? 1 : 0);
