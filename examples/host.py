#!/usr/bin/env python3
"""A minimal host: drive `benchy-mcp host` over its framed stdio protocol. Standard library only.

    python3 examples/host.py ./target/release/benchy-mcp

Attaches the bundled Arrow stream, runs the agent's kind of SQL over it, attaches several tables
in one request (protocol 2), streams a table through a pipe with no file on either side (protocol
3), names the agent's tools as the host does (protocol 4), streams tables back to back through
the session's stream channel (protocol 5), reads each result's rows and SQL without parsing the
text, and shows what a refusal looks like. The contract is docs/HOST-PROTOCOL.md. Exits non-zero if
the protocol does not do what that document says.

The session's stdin and stdout are one end of a Unix socket pair rather than pipes: a request can
then carry the read end of a pipe (SCM_RIGHTS), which is how a streamed attach hands benchy its
stream. A host that never streams can use plain pipes. The stream channel needs no socket: it is
one more pipe benchy inherits and is told of with `--stream-channel`, which is how a host that
cannot pass descriptors (Node.js, Java) streams; examples/host-node.mjs is one.
"""
import json, os, socket, struct, subprocess, sys, threading

HERE = os.path.dirname(os.path.abspath(__file__))
PLATFORM = [] if sys.platform.startswith("linux") else ["--allow-unsandboxed"]


class Host:
    def __init__(self, binary):
        self.sock, theirs = socket.socketpair()
        # The stream channel: benchy inherits the read end, at whatever number it has here.
        read, self.channel = os.pipe()
        self.p = subprocess.Popen([binary, "host", "--stream-channel", str(read), *PLATFORM],
                                  stdin=theirs, stdout=theirs, pass_fds=(read,))
        theirs.close()
        os.close(read)
        self.next = 0

    def _read(self, n):
        buf = b""
        while len(buf) < n:
            chunk = self.sock.recv(n - len(buf))
            if not chunk:
                raise EOFError("benchy closed the session")
            buf += chunk
        return buf

    def send(self, call, fds=(), **fields):
        """Send a request without waiting for its reply; `fds` ride with its bytes."""
        self.next += 1
        body = json.dumps({"id": self.next, "call": call, **fields}).encode()
        # A frame: four bytes of big-endian length, then one JSON object.
        frame = struct.pack(">I", len(body)) + body
        sent = socket.send_fds(self.sock, [frame], list(fds)) if fds else 0
        self.sock.sendall(frame[sent:])
        return self.next

    def recv(self):
        (n,) = struct.unpack(">I", self._read(4))
        return json.loads(self._read(n))

    def call(self, call, **fields):
        sent = self.send(call, **fields)
        reply = self.recv()
        assert reply["id"] == sent, reply
        return reply

    def attach_stream(self, name, chunks):
        """Attach `name` from an Arrow stream written chunk by chunk, as a host writes what it
        fetches: the read end of a pipe goes with the request, and a thread of the host's writes
        into the other end while benchy reads it. Nothing is spooled to a file."""
        read, write = os.pipe()
        sent = self.send("attach", fds=[read], name=name, stream=True)
        os.close(read)  # benchy holds its own copy now

        def writer():
            with os.fdopen(write, "wb") as w:
                try:
                    for chunk in chunks:
                        w.write(chunk)
                except BrokenPipeError:
                    pass  # benchy refused the stream and stopped reading; the reply says why

        t = threading.Thread(target=writer)
        t.start()
        reply = self.recv()
        t.join()
        assert reply["id"] == sent, reply
        return reply

    def close(self):
        self.sock.shutdown(socket.SHUT_WR)  # the session ends, and its directory goes with it
        return self.p.wait(timeout=10)


def main(binary):
    failures = []

    def check(what, ok, detail=""):
        print(f"  {'ok  ' if ok else 'FAIL'} {what}" + ("" if ok else f"  — {detail}"))
        if not ok:
            failures.append(what)

    h = Host(binary)
    # Protocol 3: a host may lower the operator's limits for its session, never raise them.
    # Protocol 4: the host's agent calls benchy's tools by the host's names, and benchy's texts
    # use them: a footer that says to read on names the host's tool, not one that does not exist.
    hello = h.call("hello", protocol=[1, 2, 3, 4, 5], limits={"attachments": 8},
                   tool_names={"get_result": "bench_page", "describe": "bench_describe"})
    check("hello agrees the newest version both speak", hello.get("protocol") == 5, hello)
    check("hello lowers the session's limits as the host asks",
          hello.get("limits", {}).get("attachments") == 8, hello)
    print(f"       {hello['engine']}, sandbox {hello['sandbox']}, caps {hello['caps']}")

    stream = os.path.join(HERE, "connectors", "hello.arrows")
    r = h.call("attach", name="readings", path=stream)
    check("attach puts an Arrow stream on the bench", r.get("ok") is True, r)
    print(f"       readings: {r.get('rows')} rows, columns {r.get('columns')}")

    sql = "SELECT meter, round(sum(power_kw), 2) AS kw FROM readings GROUP BY ALL ORDER BY kw DESC"
    r = h.call("query", sql=sql)
    check("query runs the agent's SQL", r.get("ok") is True, r)
    print("\n".join("       " + line for line in r.get("text", "").splitlines()))

    # Facts a host acts on, beside the text: each result's rows, and whether the text shows all.
    facts = r.get("results", [])
    check("query counts each result's rows beside the text",
          [f.get("result_id") for f in facts] == r.get("result_ids")
          and all(isinstance(f.get("rows"), int) for f in facts), r)

    r = h.call("page", result_id=r["result_ids"][0], offset=1)
    check("page reads a result further, without running it again", r.get("ok") is True, r)
    check("page says the SQL its result came from", r.get("sql") == sql, r)

    r = h.call("query", sql="SELECT * FROM range(500)", caps={"max_rows": 10})
    check("a host reads how much of a result its agent saw",
          r.get("results", [{}])[0].get("truncated") is True
          and r["results"][0].get("rows") == 500, r)
    check("texts name the host's tools", "bench_page" in r.get("text", "")
          and "get_result" not in r.get("text", ""), r.get("text"))

    # Protocol 2: several tables in one request, each answered on its own. Their loaders run side
    # by side, and the engine checks them once.
    r = h.call("attach", tables=[{"name": "today", "path": stream},
                                 {"name": "Yesterday", "path": stream},
                                 {"name": "tomorrow", "path": stream}])
    answers = [t.get("ok") or t.get("error", {}).get("code") for t in r.get("tables", [])]
    check("one attach puts several tables on the bench, each answered on its own",
          r.get("ok") is True and answers == [True, "E_NAME", True], r)

    # Protocol 3: a stream through a pipe, written as the host fetches it. The same table as the
    # file makes, and nothing spooled on either side.
    with open(stream, "rb") as f:
        data = f.read()
    r = h.attach_stream("streamed", (data[i:i + 1024] for i in range(0, len(data), 1024)))
    rows = h.call("describe")["tables"][0]["rows"]
    check("a streamed attach puts the same table on the bench, with no file",
          r.get("ok") is True and r.get("rows") == rows, r)
    r = h.attach_stream("cut", [data[:-8]])
    check("a stream without its end-of-stream marker is refused",
          r.get("error", {}).get("code") == "E_DATA", r)

    # Protocol 5: the stream channel. The host writes streams into it back to back, as it
    # fetches them, and each attach with "stream": "channel" takes the next whole one. A table
    # refused before or while its stream is read has its stream read on to its end and dropped,
    # so the channel stays at the next stream.
    for _ in range(3):
        os.write(h.channel, data)  # small enough for the pipe; a host writes on a thread
    r = h.call("attach", tables=[{"name": "channeled", "stream": "channel"},
                                 {"name": "Channeled", "stream": "channel"},
                                 {"name": "channeled_2", "stream": "channel"}])
    answers = [t.get("ok") or t.get("error", {}).get("code") for t in r.get("tables", [])]
    check("the channel's streams are the tables' in the order of the list",
          answers == [True, "E_NAME", True], r)
    h.call("detach", name="channeled_2")

    # Protocol 3: the host states the series key (and, where the data has one, the time column),
    # and describe says what it is, not what it looks like. A column the stream does not have is
    # refused before a row is written.
    r = h.call("attach", name="stated", path=stream, series=["meter"])
    check("an attach states the series key", r.get("ok") is True, r)
    r = h.call("attach", name="stated", path=stream, time_column="observed_at")
    check("a stated column the stream does not have is refused",
          r.get("error", {}).get("code") == "E_DATA", r)

    r = h.call("attach", name="Readings", path=stream)
    check("a name the agent could not type is refused",
          r.get("error", {}).get("code") == "E_NAME", r)
    r = h.call("query", sql="SELECT * FROM read_csv('/etc/passwd')")
    check("the bench stays sealed: file readers are refused",
          r.get("error", {}).get("code") == "E_SQL", r)
    # The message is for the model; what the host acts on is in `detail`. A batch that fails at
    # its second statement has run the first, whose result the host can page without the text.
    r = h.call("query", sql="SELECT 42 AS answer; SELECT nope")
    detail = r.get("error", {}).get("detail", {})
    check("a failed batch gives the earlier results in its detail, not only in prose",
          detail.get("statement") == 2 and len(detail.get("result_ids", [])) == 1
          and h.call("page", result_id=detail["result_ids"][0]).get("ok") is True, r)
    r = h.call("query", sql="SELECT 1", max_row=5)
    check("a field benchy does not know is an error, not ignored",
          r.get("error", {}).get("code") == "E_REQUEST", r)

    r = h.call("describe")
    check("describe lists what the host attached",
          [t["name"] for t in r.get("tables", [])] == ["readings", "today", "tomorrow",
                                                       "streamed", "channeled", "stated"], r)
    check("the session ends when the socket closes", h.close() == 0)

    print("\nall checks passed" if not failures else f"\n{len(failures)} check(s) failed")
    return 1 if failures else 0


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    sys.exit(main(sys.argv[1]))
