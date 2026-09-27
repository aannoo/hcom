//! Inline receive delivery must acknowledge only the output it emitted.
mod support;
use rusqlite::{Connection, params};
use support::Hcom;

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

fn cursor(db: &Connection, receiver: &str) -> i64 {
    db.query_row(
        "SELECT last_event_id FROM instances WHERE name=?",
        [receiver],
        |r| r.get(0),
    )
    .unwrap()
}

#[test]
fn send_delivers_one_contiguous_prefix_before_advancing_cursor() {
    for tool in ["claude", "codex", "adhoc"] {
        for count in [50, 51, 101] {
            let (h, db, sender, receiver) = setup(tool);
            let mut ids = Vec::new();
            for i in 0..count {
                send(&h, &sender, &receiver, &format!("sentinel-{i:03}-end"));
                ids.push(
                    db.query_row("SELECT MAX(id) FROM events WHERE type='message'", [], |r| {
                        r.get::<_, i64>(0)
                    })
                    .unwrap(),
                );
            }
            let first = send(&h, &receiver, &sender, "reply");
            assert_eq!(cursor(&db, &receiver), ids[49], "{tool}/{count}");
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
        send(
            &h,
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

#[test]
fn quiet_send_preserves_incoming_messages() {
    for tool in ["claude", "codex", "adhoc"] {
        let (h, db, sender, receiver) = setup(tool);
        send(&h, &sender, &receiver, "incoming-sentinel");
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

#[cfg(unix)]
#[test]
fn failed_stdout_write_preserves_incoming_messages() {
    use std::os::{fd::OwnedFd, unix::net::UnixStream};
    use std::process::Stdio;
    for tool in ["claude", "codex", "adhoc"] {
        let (h, db, sender, receiver) = setup(tool);
        send(&h, &sender, &receiver, "incoming-sentinel");
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
