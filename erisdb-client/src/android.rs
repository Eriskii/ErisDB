//! JNI surface for Android apps, bound to `dev.erisdb.client.ErisDB` so any
//! app can load it. Thin by design: strings in, JSON strings out, all real
//! work in [`crate::blocking`].
//!
//! Every entry point is wrapped in [`crate::blocking::guard`]. Unwinding
//! out of an `extern "system"` frame aborts the process, so a panic in
//! here would take the whole app down; instead it comes back as the same
//! failure value the call would have returned anyway.

use jni::objects::{JClass, JString};
use jni::sys::{jlong, jstring};
use jni::JNIEnv;

use crate::blocking::{decode_identity_hex, guard, panic_envelope};

fn jstr(env: &mut JNIEnv, s: &JString) -> String {
    env.get_string(s).map(Into::into).unwrap_or_default()
}

/// Hand a string back to the JVM. A pending exception makes every later
/// JNI call fail, so it is cleared first; if the string still cannot be
/// built — the JVM is out of memory — the answer is null, which Kotlin
/// can see, rather than a panic, which it cannot.
fn out(env: &mut JNIEnv, s: &str) -> jstring {
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_clear();
    }
    match env.new_string(s) {
        Ok(js) => js.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Connect (or reconnect) the process-wide client.
/// Returns "" on success, an error message otherwise.
#[no_mangle]
pub extern "system" fn Java_dev_erisdb_client_ErisDB_nativeConfigure(
    mut env: JNIEnv,
    _class: JClass,
    server: JString,
    token: JString,
    client_name: JString,
    identity_hex: JString,
) -> jstring {
    let message = guard(
        || {
            let server = jstr(&mut env, &server);
            let token = jstr(&mut env, &token);
            let client_name = jstr(&mut env, &client_name);
            let Some(identity) = decode_identity_hex(&jstr(&mut env, &identity_hex)) else {
                return "identity must be 64 hex characters".to_string();
            };
            match crate::blocking::configure(&server, &token, &client_name, &identity) {
                Ok(()) => String::new(),
                Err(e) => e,
            }
        },
        |panic| panic,
    );
    out(&mut env, &message)
}

/// One API call; returns the blocking facade's JSON envelope.
#[no_mangle]
pub extern "system" fn Java_dev_erisdb_client_ErisDB_nativeRequest(
    mut env: JNIEnv,
    _class: JClass,
    method: JString,
    path: JString,
    body: JString,
) -> jstring {
    let response = guard(
        || {
            let method = jstr(&mut env, &method);
            let path = jstr(&mut env, &path);
            let body = if body.is_null() { None } else { Some(jstr(&mut env, &body)) };
            crate::blocking::request(&method, &path, body.as_deref())
        },
        panic_envelope,
    );
    out(&mut env, &response)
}

/// Read a `bezel://pair/…` ticket off a QR code: returns
/// `{"ok":true,"ticket":{v, name, eid, url, token}}`, or
/// `{"ok":false,"error":…}`. Dials nothing.
#[no_mangle]
pub extern "system" fn Java_dev_erisdb_client_ErisDB_nativeParseTicket(
    mut env: JNIEnv,
    _class: JClass,
    ticket: JString,
) -> jstring {
    let response = guard(
        || {
            let ticket = jstr(&mut env, &ticket);
            crate::blocking::parse_ticket(&ticket)
        },
        panic_envelope,
    );
    out(&mut env, &response)
}

/// Redeem a ticket's code against `server`, naming this client and the
/// grants it wants (`requestedJson` is a JSON array of permissions).
/// Returns the blocking facade's JSON envelope; the answer to the request
/// comes from `nativePairPoll`.
#[no_mangle]
pub extern "system" fn Java_dev_erisdb_client_ErisDB_nativePairRedeem(
    mut env: JNIEnv,
    _class: JClass,
    server: JString,
    code: JString,
    client_name: JString,
    requested_json: JString,
    identity_hex: JString,
) -> jstring {
    let response = guard(
        || {
            let server = jstr(&mut env, &server);
            let code = jstr(&mut env, &code);
            let client_name = jstr(&mut env, &client_name);
            let requested = jstr(&mut env, &requested_json);
            let Some(identity) = decode_identity_hex(&jstr(&mut env, &identity_hex)) else {
                return crate::blocking::panic_envelope(
                    "identity must be 64 hex characters".to_string(),
                );
            };
            crate::blocking::pair_redeem(&server, &code, &client_name, &requested, &identity)
        },
        panic_envelope,
    );
    out(&mut env, &response)
}

/// Wait up to `timeoutMs` for the human's answer; returns the blocking
/// facade's JSON envelope. Call it from a background thread in a loop
/// while the status is `waiting`. An `approved` answer carries the token
/// once and only once — persist it before doing anything else.
#[no_mangle]
pub extern "system" fn Java_dev_erisdb_client_ErisDB_nativePairPoll(
    mut env: JNIEnv,
    _class: JClass,
    timeout_ms: jlong,
) -> jstring {
    let response =
        guard(|| crate::blocking::pair_poll(timeout_ms.max(0) as u64), panic_envelope);
    out(&mut env, &response)
}

/// Stop waiting and drop the pairing — the back button on a pairing
/// screen. Wakes a parked `nativePairPoll`, which answers `cancelled`.
#[no_mangle]
pub extern "system" fn Java_dev_erisdb_client_ErisDB_nativePairCancel(_env: JNIEnv, _class: JClass) {
    guard(crate::blocking::pair_cancel, |_| ());
}

/// What this client's token holds; returns the blocking facade's JSON
/// envelope. Needs no permission, so it always answers.
#[no_mangle]
pub extern "system" fn Java_dev_erisdb_client_ErisDB_nativePermissions(
    mut env: JNIEnv,
    _class: JClass,
) -> jstring {
    let response = guard(crate::blocking::permissions, panic_envelope);
    out(&mut env, &response)
}

/// Trade the current token for one with the same scope and a fresh
/// expiry; returns the blocking facade's JSON envelope. The app persists
/// the returned token itself.
#[no_mangle]
pub extern "system" fn Java_dev_erisdb_client_ErisDB_nativeRefreshCapability(
    mut env: JNIEnv,
    _class: JClass,
    ttl_secs: jlong,
) -> jstring {
    let response = guard(|| crate::blocking::refresh_capability(ttl_secs), panic_envelope);
    out(&mut env, &response)
}

/// Open a change-feed subscription from `since`; `facet` may be null for
/// the whole feed. Returns the handle, or 0 on failure.
#[no_mangle]
pub extern "system" fn Java_dev_erisdb_client_ErisDB_nativeSubscribeChanges(
    mut env: JNIEnv,
    _class: JClass,
    since: jlong,
    facet: JString,
) -> jlong {
    guard(
        || {
            let facet = if facet.is_null() { None } else { Some(jstr(&mut env, &facet)) };
            crate::blocking::subscribe_changes(since, facet.as_deref()).unwrap_or(0) as jlong
        },
        |_| 0,
    )
}

/// Block up to `timeout_ms` for the next change; returns the blocking
/// facade's JSON envelope. Call it from a background thread in a loop,
/// remembering each change's `seq` as the resume cursor.
#[no_mangle]
pub extern "system" fn Java_dev_erisdb_client_ErisDB_nativeNextChange(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    timeout_ms: jlong,
) -> jstring {
    let response = guard(
        || crate::blocking::next_change(handle as u64, timeout_ms.max(0) as u64),
        panic_envelope,
    );
    out(&mut env, &response)
}

/// Close a subscription and its stream.
#[no_mangle]
pub extern "system" fn Java_dev_erisdb_client_ErisDB_nativeCloseSubscription(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
) {
    guard(|| crate::blocking::close_subscription(handle as u64), |_| ());
}
