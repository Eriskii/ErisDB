//! JNI surface for Android apps, bound to `dev.erisdb.client.ErisDB` so any
//! app can load it. Thin by design: strings in, JSON strings out, all real
//! work in [`crate::blocking`].
//!
//! An Android app signs in through eon's bound service, asking to use
//! `database/1`; eon answers with the app's note and a snapshot of the mesh.
//! `nativeLocate` reads where the mesh's database is from that snapshot;
//! `nativeDial` dials it as the app's key; the rest is the API.
//!
//! Every entry point is wrapped in [`crate::blocking::guard`]. Unwinding
//! out of an `extern "system"` frame aborts the process, so a panic in
//! here would take the whole app down; instead it comes back as the same
//! failure value the call would have returned anyway.

use jni::objects::{JClass, JString};
use jni::sys::{jlong, jstring};
use jni::JNIEnv;

use crate::blocking::{decode_key_hex, guard, panic_envelope};

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

/// The endpoint id a key dials as, in hex — the `app` an approval asks
/// eon for; "" when the key is not 64 hex characters.
#[no_mangle]
pub extern "system" fn Java_dev_erisdb_client_ErisDB_nativeEndpointId(
    mut env: JNIEnv,
    _class: JClass,
    key_hex: JString,
) -> jstring {
    let id = guard(|| crate::blocking::endpoint_id(&jstr(&mut env, &key_hex)).unwrap_or_default(), |_| String::new());
    out(&mut env, &id)
}

/// Where the mesh's database is, from the snapshot eon answered with:
/// `{"ok":true,"server":<address JSON>}` or `{"ok":false,"error":…}`.
#[no_mangle]
pub extern "system" fn Java_dev_erisdb_client_ErisDB_nativeLocate(
    mut env: JNIEnv,
    _class: JClass,
    snapshot_json: JString,
) -> jstring {
    let response = guard(|| crate::blocking::locate(&jstr(&mut env, &snapshot_json)), panic_envelope);
    out(&mut env, &response)
}

/// Dial (or redial) the process-wide client at `server` — the address
/// `nativeLocate` gave, or a bare endpoint id — as the app holding the key.
/// Returns "" on success, an error message otherwise.
#[no_mangle]
pub extern "system" fn Java_dev_erisdb_client_ErisDB_nativeDial(
    mut env: JNIEnv,
    _class: JClass,
    server: JString,
    client_name: JString,
    key_hex: JString,
) -> jstring {
    let message = guard(
        || {
            let server = jstr(&mut env, &server);
            let client_name = jstr(&mut env, &client_name);
            let Some(key) = decode_key_hex(&jstr(&mut env, &key_hex)) else {
                return "key must be 64 hex characters".to_string();
            };
            match crate::blocking::dial(&server, &client_name, &key) {
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

/// What the app holds; returns the blocking facade's JSON envelope. Needs
/// no permission, so an enrolled app always gets an answer.
#[no_mangle]
pub extern "system" fn Java_dev_erisdb_client_ErisDB_nativePermissions(mut env: JNIEnv, _class: JClass) -> jstring {
    let response = guard(crate::blocking::permissions, panic_envelope);
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
    let response =
        guard(|| crate::blocking::next_change(handle as u64, timeout_ms.max(0) as u64), panic_envelope);
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
