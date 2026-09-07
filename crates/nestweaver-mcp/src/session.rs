//! Stdio frame ingestion stays live while one serialized dispatch owns its work.
//! Only ingestion moves to a thread; dispatch stays on the caller's thread so
//! database/config/allowlist thread-local bindings cannot disappear. Cancellation
//! is cooperative: a cancelled response is suppressed, but an admitted mutation
//! is retained until its worker actually finishes. No blocking worker is aborted.
use crate::protocol::{self, Request, error_code};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};

pub type CancelFlag = Arc<AtomicBool>;
const MAX_PENDING_REQUESTS: usize = 128;
type Pending = Arc<Mutex<HashMap<String, CancelFlag>>>;

struct CancelOnReaderFailure {
    pending: Pending,
    clean_eof: bool,
}
impl Drop for CancelOnReaderFailure {
    fn drop(&mut self) {
        if !self.clean_eof {
            for flag in self
                .pending
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .values()
            {
                flag.store(true, Ordering::Release);
            }
        }
    }
}

enum Item {
    Reply(Value),
    Dispatch(Request, CancelFlag),
}
struct Envelope {
    batch: bool,
    items: Vec<Item>,
}

fn failure(id: Value, code: i32, message: impl Into<String>) -> Value {
    json!({"jsonrpc":"2.0", "id":id, "error":{"code":code,"message":message.into()}})
}
fn key(id: &Value) -> String {
    // Envelope validation restricts numbers to lossless integers. Serialization
    // distinguishes string IDs from numbers, and never rounds the latter.
    id.to_string()
}
fn write<W: Write>(writer: &Mutex<W>, value: &Value) -> std::io::Result<()> {
    let mut writer = writer.lock().unwrap_or_else(|poison| poison.into_inner());
    let serialized = serde_json::to_string(value)?;
    crate::write_stdout_frame(&mut *writer, &serialized).map_err(|error| error.0)
}

fn admit(value: Value, pending: &Pending) -> Option<Item> {
    let req = match protocol::validate_request(value) {
        Ok(req) => req,
        Err(error) => {
            return Some(Item::Reply(failure(
                error.response_id,
                error_code::INVALID_REQUEST,
                error.message,
            )));
        }
    };
    if let Err(message) = protocol::validate_method_params(&req) {
        return req
            .id
            .map(|id| Item::Reply(failure(id, error_code::INVALID_PARAMS, message)));
    }
    if req.method == "notifications/cancelled" {
        if let Some(id) = req
            .params
            .as_ref()
            .and_then(|params| params.get("requestId"))
            && let Some(cancel) = pending
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .get(&key(id))
        {
            cancel.store(true, Ordering::Release);
        }
        return None;
    }
    // Supported notifications have no dispatch side effects. tools/call without
    // an ID was rejected above; unknown notifications are safely ignored.
    let id = req.id.as_ref()?;
    let mut active = pending.lock().unwrap_or_else(|poison| poison.into_inner());
    let id_key = key(id);
    if active.contains_key(&id_key) {
        return Some(Item::Reply(failure(
            id.clone(),
            error_code::INVALID_REQUEST,
            "request id is already in flight",
        )));
    }
    if req.method == "ping" {
        return Some(Item::Reply(json!({"jsonrpc":"2.0","id":id,"result":{}})));
    }
    if active.len() >= MAX_PENDING_REQUESTS {
        // Never block the reader on a full work queue: doing so would put
        // cancellation back behind the very work it must be able to cancel.
        return Some(Item::Reply(failure(
            id.clone(),
            error_code::INVALID_REQUEST,
            "too many pending requests; retry after an in-flight request completes",
        )));
    }
    let cancel = Arc::new(AtomicBool::new(false));
    active.insert(id_key, cancel.clone());
    Some(Item::Dispatch(req, cancel))
}

/// Serve stdio with independent control-frame ingestion. The dispatch closure
/// runs serially on THIS thread, and receives a flag for both active and queued
/// cancellation. Pass it to direct cooperative reads or `await_cancellable` for
/// read RPCs. Mutation RPCs must keep awaiting their retained daemon worker.
pub fn run_stdio(dispatch: impl FnMut(&Request, &CancelFlag) -> Value) -> anyhow::Result<()> {
    run_with_io(
        std::io::BufReader::new(std::io::stdin()),
        std::io::stdout(),
        dispatch,
    )
}

/// Injectable I/O seam for deterministic wire tests, without a real database.
pub fn run_with_io<R, W>(
    reader: R,
    writer: W,
    mut dispatch: impl FnMut(&Request, &CancelFlag) -> Value,
) -> anyhow::Result<()>
where
    R: BufRead + Send + 'static,
    W: Write + Send + 'static,
{
    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
    let output = Arc::new(Mutex::new(writer));
    let (sender, receiver) = mpsc::channel::<Result<Envelope, std::io::Error>>();
    let reader_pending = pending.clone();
    let reader_output = output.clone();
    // A detached stdin reader must not be joined on a broken stdout: stdin may
    // remain open forever after the client closes its response pipe. It exits
    // naturally on EOF or when the receiver disappears. It owns no store/worker.
    std::thread::spawn(move || {
        let mut failure_guard = CancelOnReaderFailure {
            pending: reader_pending.clone(),
            clean_eof: false,
        };
        for line in reader.lines() {
            let line = match line {
                Ok(line) => line,
                Err(error) => {
                    let _ = sender.send(Err(error));
                    return;
                }
            };
            if line.trim().is_empty() {
                continue;
            }
            let parsed: Value = match serde_json::from_str(&line) {
                Ok(value) => value,
                Err(error) => {
                    if write(
                        &reader_output,
                        &failure(
                            Value::Null,
                            error_code::PARSE_ERROR,
                            format!("invalid JSON: {error}"),
                        ),
                    )
                    .is_err()
                    {
                        return;
                    }
                    continue;
                }
            };
            let (batch, values) = match parsed {
                Value::Array(values) if values.is_empty() => {
                    if write(
                        &reader_output,
                        &failure(
                            Value::Null,
                            error_code::INVALID_REQUEST,
                            "empty batch array",
                        ),
                    )
                    .is_err()
                    {
                        return;
                    }
                    continue;
                }
                Value::Array(values) => (true, values),
                value => (false, vec![value]),
            };
            let mut items = Vec::new();
            for value in values {
                if let Some(item) = admit(value, &reader_pending) {
                    items.push(item);
                }
            }
            if items.is_empty() {
                continue;
            }
            // Invalid-only and control-only batches bypass the work queue too.
            // Every queued envelope therefore owns at least one of the bounded
            // pending dispatch slots; malformed traffic cannot queue endlessly.
            if items.iter().all(|item| matches!(item, Item::Reply(_))) {
                let values: Vec<_> = items
                    .into_iter()
                    .map(|item| match item {
                        Item::Reply(value) => value,
                        _ => unreachable!(),
                    })
                    .collect();
                let value = if batch {
                    Value::Array(values)
                } else {
                    values.into_iter().next().unwrap()
                };
                if write(&reader_output, &value).is_err() {
                    return;
                }
                continue;
            }
            if sender.send(Ok(Envelope { batch, items })).is_err() {
                return;
            }
        }
        // EOF means the client finished sending requests, not that it stopped
        // waiting for their responses. Drain the already received requests.
        failure_guard.clean_eof = true;
    });
    for envelope in receiver {
        let envelope = envelope?;
        let mut responses = Vec::new();
        for item in envelope.items {
            match item {
                Item::Reply(value) => responses.push(value),
                Item::Dispatch(req, cancel) => {
                    // Cancelled queued requests have never been admitted to a
                    // mutation worker and therefore must never be dispatched.
                    let response = if cancel.load(Ordering::Acquire) {
                        None
                    } else {
                        Some(dispatch(&req, &cancel))
                    };
                    let mut active = pending.lock().unwrap_or_else(|poison| poison.into_inner());
                    active.remove(&key(req.id.as_ref().expect("admission requires ID")));
                    if !cancel.load(Ordering::Acquire)
                        && let Some(response) = response
                    {
                        responses.push(response);
                    }
                }
            }
        }
        if responses.is_empty() {
            continue;
        }
        let value = if envelope.batch {
            Value::Array(responses)
        } else {
            responses.remove(0)
        };
        if let Err(error) = write(&output, &value) {
            for flag in pending
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .values()
            {
                flag.store(true, Ordering::Release);
            }
            if matches!(
                error.kind(),
                std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
            ) {
                return Ok(());
            }
            return Err(error.into());
        }
    }
    Ok(())
}

/// Drop a cancelled READ future so the remote handler observes disconnect and
/// signals its cooperative worker. Do not use for mutation dispatch: dropping a
/// future cannot abort spawn_blocking or undo an already committed write.
pub async fn await_cancellable<T>(
    cancel: Option<&CancelFlag>,
    future: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    let Some(cancel) = cancel else {
        return future.await;
    };
    tokio::pin!(future);
    loop {
        if cancel.load(Ordering::Acquire) {
            anyhow::bail!("request cancelled");
        }
        tokio::select! {
            result = &mut future => return result,
            _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    struct FrameWriter {
        sender: mpsc::Sender<Value>,
        buffer: Vec<u8>,
    }
    impl Write for FrameWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.buffer.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            if !self.buffer.is_empty() {
                self.sender
                    .send(serde_json::from_slice(&self.buffer).unwrap())
                    .unwrap();
                self.buffer.clear();
            }
            Ok(())
        }
    }
    fn writer() -> (FrameWriter, mpsc::Receiver<Value>) {
        let (sender, receiver) = mpsc::channel();
        (
            FrameWriter {
                sender,
                buffer: Vec::new(),
            },
            receiver,
        )
    }

    #[test]
    fn no_id_mutations_and_invalid_params_never_reach_dispatch_in_batches() {
        let (writer, output) = writer();
        let input = json!([
            {"jsonrpc":"2.0","method":"tools/call","params":{"name":"set_extension","arguments":{}}},
            {"jsonrpc":"2.0","id":2,"method":"initialize","params":[]},
            {"jsonrpc":"2.0","method":"notifications/initialized"},
            {"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"brain_status"}}
        ]).to_string();
        let mut dispatched = Vec::new();
        run_with_io(std::io::Cursor::new(input), writer, |req, _| {
            dispatched.push(req.id.clone());
            json!({"jsonrpc":"2.0","id":req.id,"result":{}})
        })
        .unwrap();
        assert_eq!(dispatched, vec![Some(json!(3))]);
        let batch = output.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(batch[0]["error"]["code"], -32600);
        assert_eq!(batch[1]["error"]["code"], -32602);
        assert_eq!(batch[2]["id"], 3);
    }

    #[test]
    fn session_preserves_line_separator_wire_framing() {
        struct Bytes(Arc<Mutex<Vec<u8>>>);
        impl Write for Bytes {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let input =
            json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"note_get"}})
                .to_string();
        run_with_io(std::io::Cursor::new(input), Bytes(bytes.clone()), |_, _| json!({"jsonrpc":"2.0","id":1,"result":{"body":"before\u{2028}middle\u{2029}after"}})).unwrap();
        let wire = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        assert!(!wire.contains(['\u{2028}', '\u{2029}']));
        let parsed: Value = serde_json::from_str(&wire).unwrap();
        assert_eq!(
            parsed["result"]["body"],
            "before\u{2028}middle\u{2029}after"
        );
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_and_ping_are_ingested_while_admitted_worker_stays_owned() {
        let (mut client, server) = std::os::unix::net::UnixStream::pair().unwrap();
        let (writer, output) = writer();
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            run_with_io(std::io::BufReader::new(server), writer, |req, cancel| {
                assert_eq!(
                    req.id,
                    Some(json!("active")),
                    "cancelled queued mutation was dispatched"
                );
                started_tx.send(()).unwrap();
                // Models a retained mutation: cancellation must not release its
                // ownership or claim to have aborted this blocking worker.
                release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                assert!(cancel.load(Ordering::Acquire));
                finished_tx.send(()).unwrap();
                json!({"jsonrpc":"2.0","id":req.id,"result":{"committed":true}})
            })
            .unwrap();
        });
        writeln!(client, "{}", json!({"jsonrpc":"2.0","id":"active","method":"tools/call","params":{"name":"set_extension"}})).unwrap();
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        for value in [
            json!({"jsonrpc":"2.0","id":"queued","method":"tools/call","params":{"name":"set_extension"}}),
            json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":"queued"}}),
            json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":"active"}}),
            json!({"jsonrpc":"2.0","id":"control","method":"ping"}),
        ] {
            writeln!(client, "{value}").unwrap();
        }
        let response = output.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(response["id"], "control", "ping waited for worker release");
        assert!(
            matches!(finished_rx.try_recv(), Err(mpsc::TryRecvError::Empty)),
            "cancellation released retained worker"
        );
        release_tx.send(()).unwrap();
        drop(client);
        thread.join().unwrap();
        finished_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            output.try_recv().is_err(),
            "cancelled request emitted a late response"
        );
    }

    #[tokio::test]
    async fn cancelled_read_drops_the_rpc_future() {
        struct Dropped(Arc<AtomicBool>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let dropped = Arc::new(AtomicBool::new(false));
        let marker = Dropped(dropped.clone());
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        let (started, waiting) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            waiting.await.unwrap();
            flag.store(true, Ordering::Release);
        });
        let result = await_cancellable(Some(&cancel), async move {
            let _marker = marker;
            started.send(()).unwrap();
            std::future::pending::<anyhow::Result<()>>().await
        })
        .await;
        assert!(result.unwrap_err().to_string().contains("cancelled"));
        assert!(
            dropped.load(Ordering::Acquire),
            "cancel only abandoned a handle, not its RPC future"
        );
    }
}
