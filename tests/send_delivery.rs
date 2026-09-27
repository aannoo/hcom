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
        "INSERT INTO events(timestamp,type,instance,data) VALUES(datetime('now'),'message',?,?)",
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
#[test]
fn send_delivers_one_contiguous_prefix_before_advancing_cursor() {
    for tool in ["claude", "codex", "adhoc"] {
        for count in [50usize, 51, 101] {
            let (h, db, sender, receiver) = setup(tool);
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
            let all = first
                + &send(&h, &receiver, &sender, "reply2")
                + &send(&h, &receiver, &sender, "reply3");
            for i in 0..count {
                assert_eq!(
                    all.matches(&format!("sentinel-{i:03}-end")).count(),
                    1,
                    "{tool}/{count}/{i}"
                );
            }
        }
    }
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
    for tool in ["codex", "adhoc"] {
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
        assert_eq!(
            status,
            if tool == "codex" {
                "active"
            } else {
                "inactive"
            }
        );
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
    for tool in ["codex", "adhoc"] {
        for external in [false, true] {
            for count in [1, 2] {
                let (h, db, sender, receiver) = setup(tool);
                for i in 0..count {
                    let data = serde_json::json!({
                        "from":"remote:BOXE", "text":format!("relay-sentinel-{i}"),
                        "scope":"mentions", "mentions":[receiver], "delivered_to":[receiver],
                        "sender_kind":"instance", "intent":"request",
                        "_relay":{"id":42+i,"short":"BOXE","device":"remote-device"}
                    });
                    db.execute("INSERT INTO events(id,timestamp,type,instance,data) VALUES(?,datetime('now'),'message','remote:BOXE',?)",params![100+i,data.to_string()]).unwrap();
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
                    assert!(!out.contains(&format!("[request #{}]", 100 + i)));
                }
                assert_eq!(cursor(&db, &receiver), 99 + count);
            }
        }
    }
}

/// A send that fails before persisting still delivers pending messages.
#[test]
fn failed_send_still_delivers_pending_messages() {
    for tool in ["codex", "adhoc"] {
        let (h, db, sender, receiver) = setup(tool);
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
        assert_ne!(code, 0, "{tool}");
        assert!(out.contains("pending-sentinel"), "{tool}: {out}");
        assert_eq!(cursor(&db, &receiver), id, "{tool}");
    }
}

/// --from delivers to the process-bound invoking instance, not only --name.
#[test]
fn external_sender_delivers_to_process_bound_instance() {
    for tool in ["codex", "adhoc"] {
        let h = Hcom::new();
        let sender = h.start();
        let process_id = format!("send-delivery-{tool}");
        let receiver = h.start_with_process_id(&process_id);
        let db = Connection::open(h.hcom_dir.join("hcom.db")).unwrap();
        db.execute(
            "UPDATE instances SET tool=? WHERE name=?",
            params![tool, receiver],
        )
        .unwrap();
        let id = queue(&db, &sender, &receiver, "bound-sentinel");
        let (code, out, err) = h.run_as_process(
            &process_id,
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
        assert_eq!(code, 0, "{tool}: {err}");
        assert!(out.contains("bound-sentinel"), "{tool}: {out}");
        assert_eq!(cursor(&db, &receiver), id, "{tool}");
    }
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
