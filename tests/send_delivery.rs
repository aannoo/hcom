//! Inline receive delivery must acknowledge only the output it emitted.
mod support;
use rusqlite::{Connection, params};
use support::Hcom;

/// Create two isolated identities and select the receiver command-routing mode.
fn setup(tool: &str) -> (Hcom, Connection, String, String) {
    let h = Hcom::new();
    let sender = h.start();
    let receiver = h.start();
    let db = Connection::open(h.hcom_dir.join("hcom.db")).unwrap();
    // Exercise command routing for each harness without launching a model.
    db.execute(
        "UPDATE instances SET tool=? WHERE name=?",
        params![tool, receiver],
    )
    .unwrap();
    (h, db, sender, receiver)
}

/// Send an informational message and capture all inline receive output.
fn send(h: &Hcom, from: &str, to: &str, body: &str) -> String {
    let (code, out, err) = h.run([
        "send",
        "--name",
        from,
        &format!("@{to}"),
        "--intent",
        "inform",
        "--",
        body,
    ]);
    assert_eq!(code, 0, "{err}");
    out
}

/// Queue a direct message by inserting its event, without spawning hcom.
fn queue(db: &Connection, from: &str, to: &str, text: &str) -> i64 {
    let data = serde_json::json!({"from":from,"text":text,"scope":"mentions","mentions":[to],"delivered_to":[to],"sender_kind":"instance","intent":"inform"});
    db.execute(
        "INSERT INTO events(timestamp,type,instance,data) VALUES(strftime('%Y-%m-%dT%H:%M:%f','now'),'message',?,?)",
        params![from, data.to_string()],
    )
    .unwrap();
    db.last_insert_rowid()
}

/// Read the persisted receive position without consuming messages.
fn cursor(db: &Connection, receiver: &str) -> i64 {
    db.query_row(
        "SELECT last_event_id FROM instances WHERE name=?",
        [receiver],
        |r| r.get(0),
    )
    .unwrap()
}

/// Verify batch boundaries and sequential exactly-once output for each routing mode.
fn check_contiguous_prefix(tool: &str) {
    let (h, db, sender, receiver) = setup(tool);
    for count in [50usize, 51, 101] {
        db.execute(
            "UPDATE instances SET last_event_id=0 WHERE name=?",
            [&receiver],
        )
        .unwrap();
        db.execute("DELETE FROM events WHERE type='message'", [])
            .unwrap();
        let ids: Vec<i64> = (0..count)
            .map(|i| queue(&db, &sender, &receiver, &format!("sentinel-{i:03}-end")))
            .collect();
        let first = send(&h, &receiver, &sender, "reply");
        assert_eq!(cursor(&db, &receiver), ids[49], "{tool}/{count}");
        assert_eq!(
            first.contains(&format!("[+{} more unread", count.saturating_sub(50))),
            count > 50,
            "{tool}/{count}: remaining note"
        );
        for i in 0..count {
            assert_eq!(
                first.contains(&format!("sentinel-{i:03}-end")),
                i < 50,
                "{tool}/{count}/{i}"
            );
        }
        let mut all = first;
        for batch in 1..count.div_ceil(50) {
            all += &send(&h, &receiver, &sender, &format!("reply-{batch}"));
        }
        assert_eq!(cursor(&db, &receiver), *ids.last().unwrap());
        let empty = send(&h, &receiver, &sender, "after-drain");
        assert!(
            !empty.contains("sentinel-"),
            "{tool}/{count}: replay after drain"
        );
        assert_eq!(cursor(&db, &receiver), *ids.last().unwrap());
        for i in 0..count {
            assert_eq!(
                all.matches(&format!("sentinel-{i:03}-end")).count(),
                1,
                "{tool}/{count}/{i}"
            );
        }
    }
}

#[test]
fn contiguous_prefix_claude() {
    check_contiguous_prefix("claude");
}

#[test]
fn contiguous_prefix_codex() {
    check_contiguous_prefix("codex");
}

#[test]
fn contiguous_prefix_adhoc() {
    check_contiguous_prefix("adhoc");
}

/// Main and child senders cannot create holes in a shared cursor prefix.
#[test]
fn mixed_main_and_child_messages_share_one_batch_limit() {
    let (h, db, sender, receiver) = setup("claude");
    let child = h.start();
    db.execute(
        "UPDATE instances SET parent_name=? WHERE name=?",
        params![receiver, child],
    )
    .unwrap();
    for i in 0..103 {
        queue(
            &db,
            if i % 3 == 0 { &child } else { &sender },
            &receiver,
            &format!("sentinel-{i:03}-end"),
        );
    }
    let first = send(&h, &receiver, &sender, "reply");
    assert!(first.contains("[Subagent messages]"));
    for i in 0..103 {
        assert_eq!(first.contains(&format!("sentinel-{i:03}-end")), i < 50);
    }
    let all =
        first + &send(&h, &receiver, &sender, "reply2") + &send(&h, &receiver, &sender, "reply3");
    for i in 0..103 {
        assert_eq!(all.matches(&format!("sentinel-{i:03}-end")).count(), 1);
    }
}

/// Quiet sends leave pending receive data available to the next command.
#[test]
fn quiet_send_preserves_incoming_messages() {
    for tool in ["claude", "codex", "adhoc"] {
        let (h, db, sender, receiver) = setup(tool);
        queue(&db, &sender, &receiver, "incoming-sentinel");
        let before = cursor(&db, &receiver);
        let (code, out, err) = h.run([
            "send",
            "--quiet",
            "--name",
            &receiver,
            &format!("@{sender}"),
            "--",
            "reply",
        ]);
        assert_eq!(code, 0, "{err}");
        assert!(out.is_empty(), "{tool}: {out}");
        assert_eq!(cursor(&db, &receiver), before);
        assert!(send(&h, &receiver, &sender, "reply2").contains("incoming-sentinel"));
    }
}

/// A failed output write does not acknowledge the queued incoming message.
#[cfg(unix)]
#[test]
fn failed_stdout_write_preserves_incoming_messages() {
    use std::os::{fd::OwnedFd, unix::net::UnixStream};
    use std::process::Stdio;
    for tool in ["claude", "codex", "adhoc"] {
        let (h, db, sender, receiver) = setup(tool);
        queue(&db, &sender, &receiver, "incoming-sentinel");
        let before = cursor(&db, &receiver);
        let (writer, reader) = UnixStream::pair().unwrap();
        drop(reader);
        let output = h
            .cmd()
            .args([
                "send",
                "--name",
                &receiver,
                &format!("@{sender}"),
                "--",
                "reply",
            ])
            .stdout(Stdio::from(OwnedFd::from(writer)))
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("Message sent, but incoming message output failed")
        );
        assert_eq!(cursor(&db, &receiver), before, "{tool}");
        assert!(send(&h, &receiver, &sender, "reply2").contains("incoming-sentinel"));
    }
}

/// An external outgoing name must not replace the invoking instance's inbox.
#[test]
fn external_sender_preserves_inline_receive_delivery() {
    for tool in ["adhoc"] {
        let (h, db, sender, receiver) = setup(tool);
        queue(&db, &sender, &receiver, "external-incoming-sentinel");
        let before = cursor(&db, &receiver);
        for mode in ["--quiet", "--json"] {
            let (code, out, err) = h.run([
                "send",
                "--name",
                &receiver,
                "--from",
                "operator",
                mode,
                &format!("@{sender}"),
                "--",
                "control",
            ]);
            assert_eq!(code, 0, "{err}");
            assert_eq!(cursor(&db, &receiver), before);
            if mode == "--quiet" {
                assert!(out.is_empty());
            } else {
                let _: serde_json::Value = serde_json::from_str(&out).unwrap();
            }
        }
        let (code, out, err) = h.run([
            "send",
            "--name",
            &receiver,
            "--from",
            "operator",
            &format!("@{sender}"),
            "--intent",
            "inform",
            "--",
            "external-outgoing",
        ]);
        assert_eq!(code, 0, "{err}");
        assert!(out.contains("external-incoming-sentinel"), "{tool}: {out}");
        let (status, context): (String, String) = db
            .query_row(
                "SELECT status,status_context FROM instances WHERE name=?",
                [&receiver],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "inactive");
        assert!(context.starts_with("deliver:"), "{context}");
        let from: String = db.query_row(
            "SELECT json_extract(data,'$.from') FROM events WHERE type='message' AND json_extract(data,'$.text')='external-outgoing'",
            [], |r| r.get(0),
        ).unwrap();
        assert_eq!(from, "operator");
    }
}

/// Hold stdout mid-write, advance the cursor elsewhere, then release the writer.
#[cfg(unix)]
#[test]
fn late_send_cannot_rewind_a_newer_cursor() {
    use std::io::Read;
    use std::os::{
        fd::{AsRawFd, OwnedFd},
        unix::net::UnixStream,
    };
    use std::process::Stdio;
    use std::time::Duration;
    let (h, db, sender, receiver) = setup("claude");
    for i in 0..51 {
        let data = serde_json::json!({"from":sender,"text":format!("sentinel-{i:03}-{}", "x".repeat(8192)),"scope":"mentions","mentions":[receiver],"delivered_to":[receiver],"sender_kind":"instance","intent":"inform"});
        db.execute("INSERT INTO events(timestamp,type,instance,data) VALUES(datetime('now'),'message',?,?)", params![sender,data.to_string()]).unwrap();
    }
    let newest: i64 = db
        .query_row("SELECT MAX(id) FROM events WHERE type='message'", [], |r| {
            r.get(0)
        })
        .unwrap();
    let before = cursor(&db, &receiver);
    let (writer, mut reader) = UnixStream::pair().unwrap();
    let size: libc::c_int = 4096;
    // Keep the socket smaller than one output batch, so its first byte proves
    // selection happened while the process still cannot finish writing.
    assert_eq!(
        unsafe {
            libc::setsockopt(
                writer.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                (&size as *const libc::c_int).cast(),
                std::mem::size_of_val(&size) as libc::socklen_t,
            )
        },
        0
    );
    reader
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut child = h
        .cmd()
        .args([
            "send",
            "--name",
            &receiver,
            &format!("@{sender}"),
            "--",
            "reply",
        ])
        .stdout(Stdio::from(OwnedFd::from(writer)))
        .spawn()
        .unwrap();
    let mut first = [0];
    reader.read_exact(&mut first).unwrap();
    assert_eq!(cursor(&db, &receiver), before);
    // Deterministic interleaving: another delivery commits the 51st event.
    db.execute(
        "UPDATE instances SET last_event_id=? WHERE name=?",
        params![newest, receiver],
    )
    .unwrap();
    let mut rest = Vec::new();
    reader.read_to_end(&mut rest).unwrap();
    assert!(child.wait().unwrap().success());
    assert_eq!(cursor(&db, &receiver), newest);
    assert!(!send(&h, &receiver, &sender, "reply2").contains("sentinel-050-"));
}

/// Relay references are reply IDs; only the cursor uses local database IDs.
#[test]
fn relay_reply_ids_survive_inline_receive() {
    // --from receives inline only for adhoc; codex gets messages via hooks.
    for (tool, external) in [("codex", false), ("adhoc", false), ("adhoc", true)] {
        {
            let (h, db, sender, receiver) = setup(tool);
            for count in [1, 2] {
                db.execute("DELETE FROM events WHERE type='message'", [])
                    .unwrap();
                db.execute(
                    "UPDATE instances SET last_event_id=0 WHERE name=?",
                    [&receiver],
                )
                .unwrap();
                let base: i64 = db
                    .query_row("SELECT COALESCE(MAX(id),0)+100 FROM events", [], |r| {
                        r.get(0)
                    })
                    .unwrap();
                for i in 0..count {
                    let data = serde_json::json!({
                        "from":"remote:BOXE", "text":format!("relay-sentinel-{i}"),
                        "scope":"mentions", "mentions":[receiver], "delivered_to":[receiver],
                        "sender_kind":"instance", "intent":"request",
                        "_relay":{"id":42+i,"short":"BOXE","device":"remote-device"}
                    });
                    db.execute("INSERT INTO events(id,timestamp,type,instance,data) VALUES(?,datetime('now'),'message','remote:BOXE',?)",params![base+i,data.to_string()]).unwrap();
                }
                let mut args = vec!["send", "--name", &receiver];
                if external {
                    args.extend(["--from", "operator"]);
                }
                let target = format!("@{sender}");
                args.extend([&target, "--", "reply"]);
                let (code, out, err) = h.run(args);
                assert_eq!(code, 0, "{err}");
                for i in 0..count {
                    assert!(
                        out.contains(&format!("[request #{}:BOXE]", 42 + i)),
                        "{tool}/{external}/{count}: {out}"
                    );
                    assert!(!out.contains(&format!("[request #{}]", base + i)));
                }
                assert_eq!(cursor(&db, &receiver), base + count - 1);
            }
        }
    }
}

/// A send that fails before persisting still delivers pending messages.
#[test]
fn failed_send_still_delivers_pending_messages() {
    let (h, db, sender, receiver) = setup("adhoc");
    let id = queue(&db, &sender, &receiver, "pending-sentinel");
    let (code, out, _) = h.run([
        "send",
        "--name",
        &receiver,
        "--intent",
        "bogus",
        &format!("@{sender}"),
        "--",
        "reply",
    ]);
    assert_ne!(code, 0);
    assert!(out.contains("pending-sentinel"), "{out}");
    assert_eq!(cursor(&db, &receiver), id);
}

/// --from delivers to the process-bound invoking instance, not only --name.
#[test]
fn external_sender_delivers_to_process_bound_instance() {
    let h = Hcom::new();
    let sender = h.start();
    let process_id = "send-delivery-adhoc";
    let receiver = h.start_with_process_id(process_id);
    let db = Connection::open(h.hcom_dir.join("hcom.db")).unwrap();
    db.execute(
        "UPDATE instances SET tool='adhoc' WHERE name=?",
        params![receiver],
    )
    .unwrap();
    let id = queue(&db, &sender, &receiver, "bound-sentinel");
    let (code, out, err) = h.run_as_process(
        process_id,
        [
            "send",
            "--from",
            "operator",
            &format!("@{sender}"),
            "--intent",
            "inform",
            "--",
            "external-outgoing",
        ],
    );
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("bound-sentinel"), "{out}");
    assert_eq!(cursor(&db, &receiver), id);
}

/// A message arriving after the batch is taken gets a notice in the same output.
#[cfg(unix)]
#[test]
fn message_arriving_mid_send_is_noticed() {
    use std::io::Read;
    use std::os::{
        fd::{AsRawFd, OwnedFd},
        unix::net::UnixStream,
    };
    use std::process::Stdio;
    use std::time::Duration;
    let (h, db, sender, receiver) = setup("adhoc");
    for i in 0..50 {
        queue(
            &db,
            &sender,
            &receiver,
            &format!("sentinel-{i:03}-{}", "x".repeat(8192)),
        );
    }
    let (writer, mut reader) = UnixStream::pair().unwrap();
    let size: libc::c_int = 4096;
    // Smaller than the batch, so the first byte arrives while send is still blocked.
    assert_eq!(
        unsafe {
            libc::setsockopt(
                writer.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                (&size as *const libc::c_int).cast(),
                std::mem::size_of_val(&size) as libc::socklen_t,
            )
        },
        0
    );
    reader
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut child = h
        .cmd()
        .args([
            "send",
            "--name",
            &receiver,
            &format!("@{sender}"),
            "--",
            "reply",
        ])
        .stdout(Stdio::from(OwnedFd::from(writer)))
        .spawn()
        .unwrap();
    let mut first = [0];
    reader.read_exact(&mut first).unwrap();
    let late = queue(&db, &sender, &receiver, "late-sentinel");
    let mut rest = String::new();
    reader.read_to_string(&mut rest).unwrap();
    assert!(child.wait().unwrap().success());
    assert!(!rest.contains("late-sentinel"));
    assert!(!rest.contains("more unread"));
    assert!(rest.contains("new message(s) arrived"), "{rest}");
    assert!(cursor(&db, &receiver) < late);
}

/// Codex has delivery hooks, so other commands and --from sends leave its
/// messages for the hooks instead of consuming them inline.
#[test]
fn codex_messages_are_left_for_hooks() {
    let (h, db, sender, receiver) = setup("codex");
    let before = cursor(&db, &receiver);
    queue(&db, &sender, &receiver, "hook-sentinel");
    let (code, out, err) = h.run(["list", "--name", &receiver]);
    assert_eq!(code, 0, "{err}");
    assert!(!out.contains("hook-sentinel"), "{out}");
    let (code, out, err) = h.run([
        "send",
        "--name",
        &receiver,
        "--from",
        "operator",
        &format!("@{sender}"),
        "--",
        "external-outgoing",
    ]);
    assert_eq!(code, 0, "{err}");
    assert!(!out.contains("hook-sentinel"), "{out}");
    assert_eq!(cursor(&db, &receiver), before);
}

/// Other adhoc commands deliver a capped prefix after their output.
#[test]
fn command_drain_delivers_capped_prefix() {
    let (h, db, sender, receiver) = setup("adhoc");
    let ids: Vec<i64> = (0..51)
        .map(|i| queue(&db, &sender, &receiver, &format!("sentinel-{i:03}-end")))
        .collect();
    let (code, out, err) = h.run(["list", "--name", &receiver]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(cursor(&db, &receiver), ids[49]);
    assert!(out.contains("sentinel-049-end") && !out.contains("sentinel-050-end"));
    assert!(out.contains("[+1 more unread"), "{out}");
    let (_, out, _) = h.run(["list", "--name", &receiver]);
    assert!(out.contains("sentinel-050-end"), "{out}");
    assert_eq!(cursor(&db, &receiver), ids[50]);
}

/// listen shows a capped prefix, notes the rest, and acknowledges only what it showed.
#[test]
fn listen_delivers_capped_prefix() {
    let (h, db, sender, receiver) = setup("adhoc");
    let ids: Vec<i64> = (0..101)
        .map(|i| queue(&db, &sender, &receiver, &format!("sentinel-{i:03}-end")))
        .collect();
    let (code, out, err) = h.run(["listen", "--name", &receiver, "--timeout", "1"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(cursor(&db, &receiver), ids[49]);
    assert!(out.contains("sentinel-049-end") && !out.contains("sentinel-050-end"));
    assert!(out.contains("[+51 more unread"), "{out}");
    // JSON stdout stays one object per line; the overflow note goes to stderr.
    let (code, out, err) = h.run(["listen", "--name", &receiver, "--json", "--timeout", "1"]);
    assert_eq!(code, 0, "{err}");
    for line in out.lines().filter(|l| !l.is_empty()) {
        let _: serde_json::Value = serde_json::from_str(line).expect(line);
    }
    assert!(out.contains("sentinel-099-end") && !out.contains("sentinel-100-end"));
    assert!(err.contains("[+1 more unread"), "{err}");
    assert_eq!(cursor(&db, &receiver), ids[99]);
}

/// A filter listen that returns on a recent match, before reading the inbox,
/// still gets the router's delivery.
#[test]
fn early_filter_listen_match_still_delivers() {
    let (h, db, sender, receiver) = setup("adhoc");
    let id = queue(&db, &sender, &receiver, "early-sentinel");
    let (code, out, err) = h.run([
        "listen",
        "--name",
        &receiver,
        "--timeout",
        "1",
        "--sql",
        "type='message'",
    ]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("[Match found]"), "{out}");
    assert!(out.contains("early-sentinel"), "{out}");
    assert_eq!(cursor(&db, &receiver), id);
}

/// A failed after-command delivery write leaves messages unread and fails the command.
#[cfg(unix)]
#[test]
fn failed_command_delivery_write_fails_command() {
    use std::os::{fd::OwnedFd, unix::net::UnixStream};
    use std::process::Stdio;
    let (h, db, sender, receiver) = setup("adhoc");
    queue(&db, &sender, &receiver, "incoming-sentinel");
    let before = cursor(&db, &receiver);
    let (writer, reader) = UnixStream::pair().unwrap();
    drop(reader);
    let output = h
        .cmd()
        .args(["config", "--name", &receiver, "timeout"])
        .stdout(Stdio::from(OwnedFd::from(writer)))
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(cursor(&db, &receiver), before);
}

/// Filter listen shows system messages it acknowledges instead of dropping them.
#[test]
fn filter_listen_shows_system_messages() {
    let (h, db, _sender, receiver) = setup("adhoc");
    let id = queue(&db, "[hcom-launcher]", &receiver, "launcher-sentinel");
    let (_, out, err) = h.run([
        "listen",
        "--name",
        &receiver,
        "--timeout",
        "1",
        "--sql",
        "type='status' AND 0",
    ]);
    assert!(out.contains("launcher-sentinel"), "{out}\n{err}");
    assert_eq!(cursor(&db, &receiver), id);
}

/// A listen that exits before reading the inbox leaves delivery to the router.
#[test]
fn listen_exiting_before_reading_still_delivers() {
    let (h, db, sender, receiver) = setup("adhoc");
    let id = queue(&db, &sender, &receiver, "unread-sentinel");
    let (_, out, err) = h.run([
        "listen",
        "--name",
        &receiver,
        "--timeout",
        "0",
        "--sql",
        "type='status' AND 0",
    ]);
    assert!(out.contains("unread-sentinel"), "{out}\n{err}");
    assert_eq!(cursor(&db, &receiver), id);
}

/// Filter listen keeps reading past a capped batch (a match may sit beyond it).
#[test]
fn filter_listen_reads_past_capped_batch() {
    let (h, db, sender, receiver) = setup("adhoc");
    let ids: Vec<i64> = (0..60)
        .map(|i| queue(&db, &sender, &receiver, &format!("sentinel-{i:03}-end")))
        .collect();
    let (_, out, err) = h.run([
        "listen",
        "--name",
        &receiver,
        "--timeout",
        "2",
        "--sql",
        "type='status' AND 0",
    ]);
    assert!(out.contains("sentinel-059-end"), "{out}\n{err}");
    assert!(!out.contains("more unread"), "{out}");
    assert_eq!(cursor(&db, &receiver), ids[59]);
}

/// A message arriving while listen writes its batch gets a notice.
#[cfg(unix)]
#[test]
fn message_arriving_mid_listen_is_noticed() {
    use std::io::Read;
    use std::os::{
        fd::{AsRawFd, OwnedFd},
        unix::net::UnixStream,
    };
    use std::process::Stdio;
    use std::time::Duration;
    let (h, db, sender, receiver) = setup("adhoc");
    for i in 0..50 {
        queue(
            &db,
            &sender,
            &receiver,
            &format!("sentinel-{i:03}-{}", "x".repeat(8192)),
        );
    }
    let (writer, mut reader) = UnixStream::pair().unwrap();
    let size: libc::c_int = 4096;
    assert_eq!(
        unsafe {
            libc::setsockopt(
                writer.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                (&size as *const libc::c_int).cast(),
                std::mem::size_of_val(&size) as libc::socklen_t,
            )
        },
        0
    );
    reader
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut child = h
        .cmd()
        .args(["listen", "--name", &receiver, "--timeout", "1"])
        .stdout(Stdio::from(OwnedFd::from(writer)))
        .spawn()
        .unwrap();
    let mut first = [0];
    reader.read_exact(&mut first).unwrap();
    let late = queue(&db, &sender, &receiver, "late-sentinel");
    let mut rest = String::new();
    reader.read_to_string(&mut rest).unwrap();
    assert!(child.wait().unwrap().success());
    assert!(!rest.contains("late-sentinel"));
    assert!(rest.contains("new message(s) arrived"), "{rest}");
    assert!(cursor(&db, &receiver) < late);
}
