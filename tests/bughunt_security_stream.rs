//! Untrusted Arrow stream regressions, exercised through the sandboxed shipped binary.
#![cfg(target_os = "linux")]

mod common;

use arrow::array::{ArrayRef, Int64Array, RecordBatch};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::ipc::writer::StreamWriter;
use common::streamed::{ChannelWriter, Streamer};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

const EOS: [u8; 8] = [255, 255, 255, 255, 0, 0, 0, 0];

fn stream(values: Vec<i64>, finish: bool) -> Vec<u8> {
    let schema = Arc::new(Schema::new(vec![Field::new(
        "value",
        DataType::Int64,
        false,
    )]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![Arc::new(Int64Array::from(values)) as ArrayRef],
    )
    .unwrap();
    let mut bytes = Vec::new();
    {
        let mut writer = StreamWriter::try_new(&mut bytes, &schema).unwrap();
        writer.write(&batch).unwrap();
        if finish {
            writer.finish().unwrap();
        }
    }
    bytes
}

fn hello(host: &mut Streamer) {
    host.sock
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    host.sock
        .set_write_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let reply = host.call("hello", json!({"protocol": [7]}), &[]);
    assert_eq!(reply["ok"], true, "{reply}");
    assert_eq!(
        reply["sandbox_posture"]["filesystem"], true,
        "the sandbox is enforced: {reply}"
    );
}

fn host() -> Streamer {
    let mut host = Streamer::spawn(&["--load-timeout", "2"]);
    hello(&mut host);
    host
}

fn marker_valued_truncated_stream() -> Vec<u8> {
    // One 64-byte values buffer ends at the message boundary. The eighth INTEGER's physical
    // bytes resemble EOS, but are data inside the batch, not a separate end marker.
    let mut values = vec![17; 8];
    values[7] = i64::from_le_bytes(EOS);
    let bytes = stream(values, false);
    assert!(
        bytes.ends_with(&EOS),
        "the final cell has the marker's bytes"
    );
    bytes
}

fn refused(reply: &Value) {
    assert_eq!(
        reply["ok"], false,
        "an unterminated stream must never attach successfully: {reply}"
    );
    assert_eq!(reply["error"]["code"], "E_DATA", "{reply}");
}

/// Checking the last eight bytes of a file accepts a truncated batch when its final value has
/// the EOS bit pattern. Completion has to follow the IPC messages, not inspect a data suffix.
#[test]
fn an_eos_bit_pattern_in_a_value_does_not_finish_an_arrow_file() {
    let path = common::unique("bughunt-fake-eos").with_extension("arrows");
    std::fs::write(&path, marker_valued_truncated_stream()).unwrap();
    let mut host = host();
    let reply = host.call("attach", json!({"name": "readings", "path": path}), &[]);
    refused(&reply);
}

/// A pipe must also require a parsed end marker: inspecting its raw tail after EOF would
/// commit the eight rows even though no EOS message was ever read.
#[test]
fn an_eos_bit_pattern_in_a_value_does_not_finish_an_arrow_pipe() {
    let mut host = host();
    let (reply, finished) = host.attach_stream("readings", marker_valued_truncated_stream());
    assert!(finished, "the small writer must have finished");
    refused(&reply);
}

/// A first stream cut at a batch boundary is followed by the next stream's schema. Its refusal
/// must close the channel, since draining to the next EOS consumes another table's stream.
#[test]
fn a_second_schema_before_eos_closes_the_channel_instead_of_eating_the_next_stream() {
    let (mut host, pipe) =
        Streamer::spawn_with_channel(&["--stream-channel", "3", "--load-timeout", "2"]);
    hello(&mut host);
    let writer = ChannelWriter::new(pipe);
    let mut bytes = stream(vec![11; 8], false);
    bytes.extend(stream(vec![22; 8], true));
    bytes.extend(stream(vec![33; 8], true));
    writer.feed(bytes);
    let first = host.call("attach", json!({"name": "first", "stream": "channel"}), &[]);
    assert_eq!(first["ok"], false, "{first}");
    let next = host.call(
        "attach",
        json!({"name": "second", "stream": "channel"}),
        &[],
    );
    assert_eq!(
        first["error"]["channel"], "closed",
        "the second schema proves the first stream lost its boundary: {first}; next attach: {next}"
    );
    assert_eq!(
        next["error"]["code"], "E_CHANNEL",
        "the third stream must not become the second table: {next}"
    );
}
