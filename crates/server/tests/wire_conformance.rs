//! Cross-language conformance vectors for the handshake wire format.
//!
//! `clients/ts/src/protocol.ts` is a hand-written mirror of
//! `crates/server/src/protocol.rs`. Nothing structural forces the two to agree:
//! each side round-trips its own bytes, so a mirror can drift into a layout
//! that is perfectly self-consistent, passes every single-language test, and
//! refuses to talk to the other side. Swapping two `u16` fields in both the
//! TypeScript encoder and the TypeScript decoder is the whole failure: it is
//! invisible to TypeScript and fatal on the wire.
//!
//! The fix is a set of bytes neither side owns. This test writes and checks
//! `tests/wire_vectors/handshake.txt`; `clients/ts/test/protocol.test.ts`
//! reads the same file, decodes every frame, asserts the fields it got, and
//! re-encodes to the same bytes. A layout divergence fails on one side or the
//! other, by construction.
//!
//! The file also pins the feature-name list and the error-class numbering,
//! which are the other two places the two implementations (and the driver
//! documentation) can silently disagree.
//!
//! Regenerate after an intentional wire change:
//!
//! ```text
//! POWDB_UPDATE_WIRE_VECTORS=1 cargo test -p powdb-server --test wire_conformance
//! ```
//!
//! Regenerating is not a way to make a failure go away. It only restates what
//! Rust does; the TypeScript half then fails until it is brought along, which
//! is the point.

use std::fmt::Write as _;
use std::path::PathBuf;

use powdb_server::protocol::{
    decode_error_class, ClientHello, ErrorClass, Message, ServerHello, SERVER_FEATURES,
};

fn vectors_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/wire_vectors/handshake.txt")
}

fn docs_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/integrations/powql-for-drivers.md")
}

fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn from_hex(hex: &str) -> Vec<u8> {
    assert!(hex.len().is_multiple_of(2), "odd-length hex: {hex}");
    hex.as_bytes()
        .chunks(2)
        .map(|pair| {
            let text = std::str::from_utf8(pair).expect("hex is ascii");
            u8::from_str_radix(text, 16).unwrap_or_else(|_| panic!("bad hex byte {text}"))
        })
        .collect()
}

/// The canonical handshake frames.
///
/// Every value here is a literal. Nothing depends on the crate version or on
/// [`SERVER_FEATURES`], so a vector's bytes change only when the wire format
/// itself changes, never because a release added a feature name.
fn frames() -> Vec<(&'static str, Message)> {
    let sample_features = || vec!["params".to_owned(), "sql".to_owned()];
    vec![
        (
            "connect_legacy_password_only",
            Message::Connect {
                db_name: "mydb".into(),
                password: Some("pw".to_owned().into()),
                username: None,
            },
        ),
        (
            "connect_legacy_password_and_username",
            Message::Connect {
                db_name: "mydb".into(),
                password: Some("pw".to_owned().into()),
                username: Some("alice".into()),
            },
        ),
        (
            "connect_hello_no_credentials",
            Message::ConnectWithHello {
                db_name: "mydb".into(),
                password: None,
                username: None,
                hello: ClientHello {
                    min_protocol: 1,
                    max_protocol: 2,
                    catalog_version: 7,
                    features: Vec::new(),
                },
            },
        ),
        (
            "connect_hello_full",
            Message::ConnectWithHello {
                db_name: "mydb".into(),
                password: Some("pw".to_owned().into()),
                username: Some("alice".into()),
                hello: ClientHello {
                    min_protocol: 1,
                    max_protocol: 2,
                    catalog_version: 7,
                    features: sample_features(),
                },
            },
        ),
        (
            "connect_ok_legacy",
            Message::ConnectOk {
                version: "0.21.0".into(),
            },
        ),
        (
            "connect_ok_hello",
            Message::ConnectOkWithHello {
                version: "0.22.0".into(),
                hello: ServerHello {
                    protocol: 2,
                    min_protocol: 1,
                    max_protocol: 2,
                    catalog_version: 7,
                    features: sample_features(),
                },
            },
        ),
        (
            "connect_ok_hello_no_features",
            Message::ConnectOkWithHello {
                version: "0.22.0".into(),
                hello: ServerHello {
                    protocol: 1,
                    min_protocol: 1,
                    max_protocol: 2,
                    catalog_version: 7,
                    features: Vec::new(),
                },
            },
        ),
        (
            "error_protocol_version",
            Message::ErrorWithClass {
                message: "unsupported wire protocol".into(),
                class: ErrorClass::ProtocolVersion,
            },
        ),
    ]
}

/// The `(db_name, password, username)` a CONNECT frame must decode to.
type ConnectFields = (&'static str, Option<&'static str>, Option<&'static str>);

/// Byte shapes this build never emits but must still accept: the vector name,
/// its hex bytes, and the fields it has to decode to.
///
/// A real v0.21.0 client omits trailing optional fields entirely rather than
/// writing them as empty strings. These are the exact bytes such a client puts
/// on the wire, hand-written so they cannot drift with the encoder.
fn alternate_frames() -> Vec<(&'static str, &'static str, ConnectFields)> {
    vec![
        // db_name only: the oldest CONNECT shape of all.
        (
            "connect_legacy_db_only",
            "010008000000040000006d796462",
            ("mydb", None, None),
        ),
        // db_name + password + an explicitly empty username. This is what
        // PowDB's own v0.21.0 encoder wrote for a client with no named user,
        // so it has to keep decoding forever even though nothing emits it now.
        (
            "connect_legacy_username_explicit_empty",
            "010012000000040000006d79646202000000707700000000",
            ("mydb", Some("pw"), None),
        ),
    ]
}

/// Every [`ErrorClass`] and its wire byte, in numeric order.
fn error_classes() -> Vec<(&'static str, u8)> {
    vec![
        ("internal", ErrorClass::Internal.as_u8()),
        ("parse", ErrorClass::Parse.as_u8()),
        ("execution", ErrorClass::Execution.as_u8()),
        ("timeout", ErrorClass::Timeout.as_u8()),
        ("limit_exceeded", ErrorClass::LimitExceeded.as_u8()),
        ("readonly_refused", ErrorClass::ReadonlyRefused.as_u8()),
        ("auth_failed", ErrorClass::AuthFailed.as_u8()),
        ("rate_limited", ErrorClass::RateLimited.as_u8()),
        (
            "constraint_violation",
            ErrorClass::ConstraintViolation.as_u8(),
        ),
        ("cancelled", ErrorClass::Cancelled.as_u8()),
        ("protocol_version", ErrorClass::ProtocolVersion.as_u8()),
    ]
}

fn render_vectors() -> String {
    let mut out = String::new();
    out.push_str(
        "# PowDB handshake wire vectors. GENERATED - see crates/server/tests/wire_conformance.rs.\n\
         #\n\
         # These bytes are the contract between crates/server/src/protocol.rs and\n\
         # clients/ts/src/protocol.ts. Both decode every frame here, assert the fields\n\
         # they got, and re-encode to the same bytes, so a layout that drifts in one\n\
         # implementation fails in the other instead of shipping.\n\
         #\n\
         # Records:\n\
         #   frame     <name> <hex>  one complete wire frame, header included\n\
         #   framealt  <name> <hex>  a byte shape this build never emits but must\n\
         #                           still decode (what an older client writes)\n\
         #   feature   <name>        a wire feature name, in SERVER_FEATURES order\n\
         #   class     <name> <byte> an error class and its wire byte\n\
         #\n\
         # Regenerate: POWDB_UPDATE_WIRE_VECTORS=1 cargo test -p powdb-server --test wire_conformance\n\
         \n",
    );
    for (name, msg) in frames() {
        let _ = writeln!(out, "frame {name} {}", to_hex(&msg.encode()));
    }
    out.push('\n');
    for (name, hex, _) in alternate_frames() {
        let _ = writeln!(out, "framealt {name} {hex}");
    }
    out.push('\n');
    for feature in SERVER_FEATURES {
        let _ = writeln!(out, "feature {feature}");
    }
    out.push('\n');
    for (name, byte) in error_classes() {
        let _ = writeln!(out, "class {name} {byte}");
    }
    out
}

/// Records of one kind, as `(name, rest)` pairs in file order.
fn records(text: &str, kind: &str) -> Vec<(String, String)> {
    text.lines()
        .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            if parts.next()? != kind {
                return None;
            }
            let name = parts.next()?.to_owned();
            Ok::<_, ()>((name, parts.collect::<Vec<_>>().join(" "))).ok()
        })
        .collect()
}

fn load_vectors() -> String {
    let path = vectors_path();
    let rendered = render_vectors();
    if std::env::var_os("POWDB_UPDATE_WIRE_VECTORS").is_some() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create vector directory");
        }
        std::fs::write(&path, &rendered).expect("write vectors");
        return rendered;
    }
    std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "cannot read {}: {e}. Regenerate with \
             POWDB_UPDATE_WIRE_VECTORS=1 cargo test -p powdb-server --test wire_conformance",
            path.display()
        )
    })
}

#[test]
fn committed_vectors_match_what_this_build_encodes() {
    let on_disk = load_vectors();
    assert_eq!(
        on_disk,
        render_vectors(),
        "the committed wire vectors no longer match this build's encoder. If the \
         wire format changed on purpose, regenerate with \
         POWDB_UPDATE_WIRE_VECTORS=1 cargo test -p powdb-server --test wire_conformance \
         and bring clients/ts/src/protocol.ts along; the TypeScript half of this \
         contract will fail until you do."
    );
}

#[test]
fn every_committed_frame_decodes_back_to_the_message_that_produced_it() {
    let text = load_vectors();
    let on_disk = records(&text, "frame");
    let expected = frames();
    assert_eq!(
        on_disk.len(),
        expected.len(),
        "vector file has {} frames, this build defines {}",
        on_disk.len(),
        expected.len()
    );
    for ((disk_name, hex), (name, msg)) in on_disk.iter().zip(expected) {
        assert_eq!(disk_name, name, "frame vectors are order-sensitive");
        let bytes = from_hex(hex);
        assert_eq!(bytes, msg.encode(), "encoding drift for frame '{name}'");
        let decoded = Message::decode(&bytes).unwrap_or_else(|e| panic!("frame '{name}': {e}"));
        match (&msg, &decoded) {
            // The class byte rides outside the decoded variant, so an Error
            // frame decodes to the plain variant. Check the class separately
            // rather than skipping it.
            (Message::ErrorWithClass { message, class }, Message::Error { message: got }) => {
                assert_eq!(got, message, "frame '{name}'");
                assert_eq!(
                    decode_error_class(&bytes),
                    Some(class.as_u8()),
                    "frame '{name}' must carry its class byte"
                );
            }
            // `Message` has no `PartialEq`, and re-encoding is the stronger
            // check anyway: it fails if the decoder dropped, reordered, or
            // invented any field the encoder writes.
            _ => assert_eq!(
                decoded.encode(),
                bytes,
                "frame '{name}' did not survive decode then re-encode"
            ),
        }
    }
}

#[test]
fn the_byte_shapes_an_older_client_writes_still_decode() {
    let text = load_vectors();
    let on_disk = records(&text, "framealt");
    let expected = alternate_frames();
    assert_eq!(
        on_disk.len(),
        expected.len(),
        "vector file has {} alternate frames, this build defines {}",
        on_disk.len(),
        expected.len()
    );
    for ((disk_name, hex), (name, expected_hex, (db, password, username))) in
        on_disk.iter().zip(expected)
    {
        assert_eq!(disk_name, name, "alternate vectors are order-sensitive");
        assert_eq!(hex, expected_hex, "alternate frame '{name}' bytes changed");
        match Message::decode(&from_hex(hex)) {
            Ok(Message::Connect {
                db_name,
                password: got_password,
                username: got_username,
            }) => {
                assert_eq!(db_name, db, "alternate frame '{name}' db name");
                assert_eq!(
                    got_password.as_deref().map(|p| p.as_str()),
                    password,
                    "alternate frame '{name}' password"
                );
                assert_eq!(
                    got_username.as_deref(),
                    username,
                    "alternate frame '{name}' username"
                );
            }
            Ok(other) => panic!("alternate frame '{name}' decoded as {other:?}"),
            Err(e) => panic!("alternate frame '{name}' must decode: {e}"),
        }
    }
}

#[test]
fn the_committed_feature_list_is_this_build_feature_list() {
    let text = load_vectors();
    let on_disk: Vec<String> = records(&text, "feature")
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert_eq!(
        on_disk, SERVER_FEATURES,
        "the wire feature list changed. Regenerate the vectors and add the name to \
         WIRE_FEATURE in clients/ts/src/protocol.ts, CLIENT_CAPABILITIES.features in \
         clients/ts/src/index.ts, and the feature table in \
         docs/integrations/powql-for-drivers.md"
    );
}

#[test]
fn the_committed_error_classes_are_this_build_error_classes() {
    let text = load_vectors();
    let on_disk = records(&text, "class");
    let expected = error_classes();
    assert_eq!(on_disk.len(), expected.len(), "error class count changed");
    for ((disk_name, disk_byte), (name, byte)) in on_disk.iter().zip(expected) {
        assert_eq!(disk_name, name, "error classes are order-sensitive");
        assert_eq!(
            disk_byte.parse::<u8>().expect("class byte is numeric"),
            byte,
            "wire byte for error class '{name}' changed; renumbering is a protocol break"
        );
        assert_eq!(
            ErrorClass::from_u8(byte).map(|c| c.as_u8()),
            Some(byte),
            "class '{name}' must decode from its own byte"
        );
    }
}

/// The driver documentation is the only spec a third-party client has. A
/// feature that exists in the server and not in that table is a feature no
/// external driver can know to ask for, so the table is checked like code.
#[test]
fn the_driver_docs_feature_table_lists_exactly_the_shipped_features() {
    let docs = std::fs::read_to_string(docs_path()).expect("read driver docs");
    let (_, after) = docs
        .split_once("#### Named features")
        .expect("driver docs must document the named features");
    let documented: Vec<String> = after
        .lines()
        .skip_while(|line| !line.starts_with('|'))
        .take_while(|line| line.starts_with('|'))
        .filter_map(|line| {
            let cell = line.split('|').nth(1)?.trim();
            let name = cell.trim_matches('`');
            // Skip the header row and the `|---|---|` separator.
            if name == "Name" || name.chars().all(|c| c == '-') || name.is_empty() {
                return None;
            }
            Some(name.to_owned())
        })
        .collect();
    assert_eq!(
        documented, SERVER_FEATURES,
        "the named-feature table in docs/integrations/powql-for-drivers.md does not \
         match SERVER_FEATURES; a driver author cannot ask for a feature that is not \
         documented"
    );
}
