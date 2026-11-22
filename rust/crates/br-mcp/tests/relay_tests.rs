// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use br_contracts::InstallLayout;
use br_integrity::INTEGRITY_EXIT_CODE;
use br_mcp::{
    ChildHandles, ChildWaiter, RelayConfig, RelayDeps, StartError, message_id, resolve_install_dir,
    run_relay,
};
use serde_json::Value;

/// In-memory byte pipe connecting a writer to a blocking reader.
struct PipeBuf {
    data: VecDeque<u8>,
    closed: bool,
}

struct PipeWriter {
    inner: Arc<(Mutex<PipeBuf>, Condvar)>,
}

struct PipeReader {
    inner: Arc<(Mutex<PipeBuf>, Condvar)>,
}

fn mem_pipe() -> (PipeWriter, PipeReader) {
    let inner = Arc::new((
        Mutex::new(PipeBuf {
            data: VecDeque::new(),
            closed: false,
        }),
        Condvar::new(),
    ));
    (
        PipeWriter {
            inner: Arc::clone(&inner),
        },
        PipeReader { inner },
    )
}

impl Write for PipeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let (lock, cv) = &*self.inner;
        let mut st = lock.lock().unwrap();
        if st.closed {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"));
        }
        st.data.extend(buf);
        cv.notify_all();
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for PipeWriter {
    fn drop(&mut self) {
        let (lock, cv) = &*self.inner;
        let mut st = lock.lock().unwrap();
        st.closed = true;
        cv.notify_all();
    }
}

impl Read for PipeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let (lock, cv) = &*self.inner;
        let mut st = lock.lock().unwrap();
        while st.data.is_empty() && !st.closed {
            st = cv.wait(st).unwrap();
        }
        if st.data.is_empty() {
            return Ok(0);
        }
        let n = buf.len().min(st.data.len());
        for slot in buf.iter_mut().take(n) {
            *slot = st.data.pop_front().unwrap();
        }
        Ok(n)
    }
}

struct ChannelWaiter {
    rx: mpsc::Receiver<i32>,
    exited: Option<i32>,
}

impl ChildWaiter for ChannelWaiter {
    fn try_wait(&mut self) -> io::Result<Option<i32>> {
        if let Some(code) = self.exited {
            return Ok(Some(code));
        }
        match self.rx.try_recv() {
            Ok(code) => {
                self.exited = Some(code);
                Ok(Some(code))
            }
            Err(mpsc::TryRecvError::Empty) => Ok(None),
            Err(mpsc::TryRecvError::Disconnected) => {
                self.exited = Some(0);
                Ok(Some(0))
            }
        }
    }
}

/// Spawns an in-process fake MCP server driven by `handler(line) -> (Option<reply>, Option<exit_code>)`.
fn script_child<F>(mut handler: F) -> ChildHandles
where
    F: FnMut(&str) -> (Option<String>, Option<i32>) + Send + 'static,
{
    let (stdin_w, stdin_r) = mem_pipe();
    let (mut stdout_w, stdout_r) = mem_pipe();
    let (exit_tx, exit_rx) = mpsc::channel();

    thread::spawn(move || {
        use std::io::BufRead;
        let mut reader = io::BufReader::new(stdin_r);
        let mut line = String::new();
        let code = loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break 0,
                Ok(_) => {
                    let trimmed = line.trim_end_matches(['\r', '\n']);
                    let (reply, exit) = handler(trimmed);
                    if let Some(r) = reply {
                        let _ = writeln!(stdout_w, "{r}");
                        let _ = stdout_w.flush();
                    }
                    if let Some(c) = exit {
                        break c;
                    }
                }
            }
        };
        drop(stdout_w);
        let _ = exit_tx.send(code);
    });

    ChildHandles {
        stdin: Box::new(stdin_w),
        stdout: Box::new(stdout_r),
        waiter: Box::new(ChannelWaiter {
            rx: exit_rx,
            exited: None,
        }),
    }
}

fn fast_config() -> RelayConfig {
    RelayConfig {
        mutex_poll: Duration::from_millis(20),
        mutex_max_wait: Duration::from_secs(5),
        mutex_grace: Duration::from_millis(500),
        child_poll: Duration::from_millis(10),
        restart_window: Duration::from_secs(60),
        max_restarts: 5,
    }
}

fn read_json_line(reader: &mut io::BufReader<PipeReader>) -> Value {
    use std::io::BufRead;
    let mut line = String::new();
    let n = reader.read_line(&mut line).expect("read reply");
    assert!(n > 0, "unexpected EOF from relay");
    serde_json::from_str(&line).expect("valid JSON line")
}

#[test]
fn message_id_tells_requests_from_responses() {
    assert_eq!(
        message_id(b"{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"ping\"}\n", true),
        Some(("n:7".to_string(), "7".to_string()))
    );
    assert_eq!(
        message_id(b"{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"ping\"}\n", false),
        None
    );
    assert_eq!(
        message_id(b"{\"jsonrpc\":\"2.0\",\"id\":\"7\",\"result\":{}}\n", false),
        Some(("s:7".to_string(), "\"7\"".to_string()))
    );
    assert_eq!(
        message_id(
            b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n",
            true
        ),
        None
    );
    assert_eq!(message_id(b"{nope\n", true), None);
    let mut bom = vec![0xEF, 0xBB, 0xBF];
    bom.extend_from_slice(b"{\"id\":1,\"method\":\"x\"}\n");
    assert_eq!(
        message_id(&bom, true),
        Some(("n:1".to_string(), "1".to_string()))
    );
    // Non-string method is rejected for both request and response.
    assert_eq!(
        message_id(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":5}\n", true),
        None
    );
    assert_eq!(
        message_id(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":5}\n", false),
        None
    );
}

#[test]
fn resolve_install_dir_order() {
    let args = vec![
        "--install".to_string(),
        r"C:\custom\br".to_string(),
        "--extra".to_string(),
    ];
    let (dir, rest) = resolve_install_dir(
        &args,
        || Some(PathBuf::from(r"C:\reg\br")),
        || PathBuf::from(r"C:\default\br"),
    )
    .unwrap();
    assert_eq!(dir, PathBuf::from(r"C:\custom\br"));
    assert_eq!(rest, vec!["--extra".to_string()]);

    let (dir, rest) = resolve_install_dir(
        &["--foo".to_string()],
        || Some(PathBuf::from(r"C:\reg\br")),
        || PathBuf::from(r"C:\default\br"),
    )
    .unwrap();
    assert_eq!(dir, PathBuf::from(r"C:\reg\br"));
    assert_eq!(rest, vec!["--foo".to_string()]);

    let (dir, _) = resolve_install_dir(&[], || None, || PathBuf::from(r"C:\default\br")).unwrap();
    assert_eq!(dir, PathBuf::from(r"C:\default\br"));

    assert!(
        resolve_install_dir(
            &["--install".to_string()],
            || None,
            || PathBuf::from(r"C:\default\br")
        )
        .is_err()
    );
}

#[test]
fn unanswered_requests_are_resent_after_exit_75_and_notifications_are_not() {
    let (mut client_w, relay_in) = mem_pipe();
    let (relay_out, client_r) = mem_pipe();
    let mut client_reader = io::BufReader::new(client_r);

    let second_saw = Arc::new(Mutex::new(Vec::<String>::new()));
    let second_saw_clone = Arc::clone(&second_saw);
    let generation = Arc::new(Mutex::new(0usize));

    let relay_thread = thread::spawn(move || {
        let mut stderr = Vec::new();
        run_relay(RelayDeps {
            client_in: relay_in,
            client_out: relay_out,
            stderr: &mut stderr,
            install_dir: PathBuf::from(r"C:\fake\install"),
            mutex_exists: || false,
            start_child: move |_: &Path| {
                let mut g = generation.lock().unwrap();
                *g += 1;
                if *g == 1 {
                    Ok(script_child(|line| {
                        let v: Value = serde_json::from_str(line).unwrap();
                        if v.get("id").is_none() {
                            return (None, None);
                        }
                        let id = v["id"].as_i64().unwrap();
                        if id == 1 {
                            (Some(r#"{"jsonrpc":"2.0","id":1,"result":{"ok":1}}"#.to_string()), None)
                        } else {
                            (None, Some(InstallLayout::MCP_RESTART_EXIT_CODE))
                        }
                    }))
                } else {
                    let seen = Arc::clone(&second_saw_clone);
                    Ok(script_child(move |line| {
                        seen.lock().unwrap().push(line.to_string());
                        let v: Value = serde_json::from_str(line).unwrap();
                        let id = v["id"].as_i64().unwrap();
                        (
                            Some(format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{{"ok":2}}}}"#)),
                            None,
                        )
                    }))
                }
            },
            config: fast_config(),
        })
    });

    writeln!(client_w, r#"{{"jsonrpc":"2.0","id":1,"method":"ping"}}"#).unwrap();
    let r1 = read_json_line(&mut client_reader);
    assert_eq!(r1["id"].as_i64(), Some(1));

    // Send a notification then a request that causes child 1 to exit 75.
    writeln!(
        client_w,
        r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#
    )
    .unwrap();
    writeln!(client_w, r#"{{"jsonrpc":"2.0","id":2,"method":"ping"}}"#).unwrap();
    let r2 = read_json_line(&mut client_reader);
    assert_eq!(r2["id"].as_i64(), Some(2));
    assert_eq!(r2["result"]["ok"].as_i64(), Some(2));

    drop(client_w);
    let outcome = relay_thread.join().unwrap();
    assert_eq!(outcome.exit_code, 0);
    assert_eq!(outcome.restarts, 1);

    // Child 2 saw ONLY request id 2, NOT id 1 and NOT the notification.
    let saw = second_saw.lock().unwrap();
    assert_eq!(saw.len(), 1);
    assert!(saw[0].contains(r#""id":2"#));
}

#[test]
fn restart_waits_while_setup_mutex_exists() {
    let (mut client_w, relay_in) = mem_pipe();
    let (relay_out, client_r) = mem_pipe();
    let mut client_reader = io::BufReader::new(client_r);

    let mutex_held = Arc::new(AtomicBool::new(false));
    let mutex_check = Arc::clone(&mutex_held);
    let started_count = Arc::new(Mutex::new(0usize));
    let started_ref = Arc::clone(&started_count);

    let relay_thread = thread::spawn(move || {
        let mut stderr = Vec::new();
        run_relay(RelayDeps {
            client_in: relay_in,
            client_out: relay_out,
            stderr: &mut stderr,
            install_dir: PathBuf::from(r"C:\fake\install"),
            mutex_exists: move || mutex_check.load(Ordering::Acquire),
            start_child: move |_: &Path| {
                let mut c = started_ref.lock().unwrap();
                *c += 1;
                if *c == 1 {
                    Ok(script_child(|_| (None, Some(InstallLayout::MCP_RESTART_EXIT_CODE))))
                } else {
                    Ok(script_child(|line| {
                        let v: Value = serde_json::from_str(line).unwrap();
                        let id = v["id"].as_i64().unwrap();
                        (
                            Some(format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{{}}}}"#)),
                            None,
                        )
                    }))
                }
            },
            config: fast_config(),
        })
    });

    // Let the first server start before the "installer" takes the mutex.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while *started_count.lock().unwrap() == 0 {
        assert!(std::time::Instant::now() < deadline, "first server never started");
        thread::sleep(Duration::from_millis(5));
    }
    mutex_held.store(true, Ordering::Release);
    writeln!(client_w, r#"{{"jsonrpc":"2.0","id":10,"method":"ping"}}"#).unwrap();
    thread::sleep(Duration::from_millis(120));
    // Still waiting for mutex to clear: second server has not started yet.
    assert_eq!(*started_count.lock().unwrap(), 1);

    mutex_held.store(false, Ordering::Release);
    let r = read_json_line(&mut client_reader);
    assert_eq!(r["id"].as_i64(), Some(10));

    drop(client_w);
    let outcome = relay_thread.join().unwrap();
    assert_eq!(outcome.exit_code, 0);
    assert_eq!(outcome.restarts, 1);
}

#[test]
fn non_75_exit_without_mutex_ends_relay_and_fails_pending() {
    let (mut client_w, relay_in) = mem_pipe();
    let (relay_out, client_r) = mem_pipe();
    let mut client_reader = io::BufReader::new(client_r);

    let relay_thread = thread::spawn(move || {
        let mut stderr = Vec::new();
        run_relay(RelayDeps {
            client_in: relay_in,
            client_out: relay_out,
            stderr: &mut stderr,
            install_dir: PathBuf::from(r"C:\fake\install"),
            mutex_exists: || false,
            start_child: |_: &Path| Ok(script_child(|_| (None, Some(3)))),
            config: fast_config(),
        })
    });

    writeln!(client_w, r#"{{"jsonrpc":"2.0","id":9,"method":"ping"}}"#).unwrap();
    let r = read_json_line(&mut client_reader);
    assert_eq!(r["id"].as_i64(), Some(9));
    assert_eq!(r["error"]["code"].as_i64(), Some(-32603));
    assert!(
        r["error"]["message"]
            .as_str()
            .unwrap()
            .contains("stopped")
    );

    let outcome = relay_thread.join().unwrap();
    assert_eq!(outcome.exit_code, 3);
    assert_eq!(outcome.restarts, 0);
}

#[test]
fn non_75_exit_restarts_when_mutex_appeared_recently() {
    let (mut client_w, relay_in) = mem_pipe();
    let (relay_out, client_r) = mem_pipe();
    let mut client_reader = io::BufReader::new(client_r);

    let mutex_held = Arc::new(AtomicBool::new(false));
    let mutex_check = Arc::clone(&mutex_held);
    let mutex_in_child = Arc::clone(&mutex_held);
    let generation = Arc::new(Mutex::new(0usize));

    let relay_thread = thread::spawn(move || {
        let mut stderr = Vec::new();
        run_relay(RelayDeps {
            client_in: relay_in,
            client_out: relay_out,
            stderr: &mut stderr,
            install_dir: PathBuf::from(r"C:\fake\install"),
            mutex_exists: move || mutex_check.load(Ordering::Acquire),
            start_child: move |_: &Path| {
                let mut g = generation.lock().unwrap();
                *g += 1;
                if *g == 1 {
                    let mf = Arc::clone(&mutex_in_child);
                    Ok(script_child(move |_| {
                        // Simulate installer holding mutex and terminating server with non-75 code.
                        mf.store(true, Ordering::Release);
                        thread::sleep(Duration::from_millis(40));
                        mf.store(false, Ordering::Release);
                        (None, Some(1))
                    }))
                } else {
                    Ok(script_child(|line| {
                        let v: Value = serde_json::from_str(line).unwrap();
                        let id = v["id"].as_i64().unwrap();
                        (
                            Some(format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{{"recovered":true}}}}"#)),
                            None,
                        )
                    }))
                }
            },
            config: fast_config(),
        })
    });

    writeln!(client_w, r#"{{"jsonrpc":"2.0","id":42,"method":"ping"}}"#).unwrap();
    let r = read_json_line(&mut client_reader);
    assert_eq!(r["id"].as_i64(), Some(42));
    assert_eq!(r["result"]["recovered"].as_bool(), Some(true));

    drop(client_w);
    let outcome = relay_thread.join().unwrap();
    assert_eq!(outcome.exit_code, 0);
    assert_eq!(outcome.restarts, 1);
}

#[test]
fn integrity_failure_exits_86_with_stderr_line() {
    let (_client_w, relay_in) = mem_pipe();
    let (relay_out, _client_r) = mem_pipe();
    let mut stderr = Vec::new();

    let outcome = run_relay(RelayDeps {
        client_in: relay_in,
        client_out: relay_out,
        stderr: &mut stderr,
        install_dir: PathBuf::from(r"C:\fake\install"),
        mutex_exists: || false,
        start_child: |_: &Path| Err(StartError::Integrity("tampered file".to_string())),
        config: fast_config(),
    });
    assert_eq!(outcome.exit_code, INTEGRITY_EXIT_CODE);
    let err = String::from_utf8(stderr).unwrap();
    assert!(err.starts_with("betterrecall-mcp: "));
    assert!(err.contains("tampered file"));
}
