//! JNI surface for the Android lists app. Thin by design: strings in,
//! JSON strings out, all real work in [`crate::blocking`].

use jni::objects::{JClass, JString};
use jni::sys::{jlong, jstring};
use jni::JNIEnv;

fn jstr(env: &mut JNIEnv, s: &JString) -> String {
    env.get_string(s).map(Into::into).unwrap_or_default()
}

fn out(env: &JNIEnv, s: &str) -> jstring {
    env.new_string(s).expect("jvm string").into_raw()
}

/// Connect (or reconnect) the process-wide client.
/// Returns "" on success, an error message otherwise.
#[no_mangle]
pub extern "system" fn Java_com_example_bezellists_Bezel_nativeConfigure(
    mut env: JNIEnv,
    _class: JClass,
    server: JString,
    token: JString,
    client_name: JString,
    identity_hex: JString,
) -> jstring {
    let server = jstr(&mut env, &server);
    let token = jstr(&mut env, &token);
    let client_name = jstr(&mut env, &client_name);
    let identity = match decode_hex(&jstr(&mut env, &identity_hex)) {
        Some(bytes) => bytes,
        None => return out(&env, "identity must be 64 hex chars"),
    };
    match crate::blocking::configure(&server, &token, &client_name, &identity) {
        Ok(()) => out(&env, ""),
        Err(e) => out(&env, &e),
    }
}

/// One API call; returns the blocking facade's JSON envelope.
#[no_mangle]
pub extern "system" fn Java_com_example_bezellists_Bezel_nativeRequest(
    mut env: JNIEnv,
    _class: JClass,
    method: JString,
    path: JString,
    body: JString,
) -> jstring {
    let method = jstr(&mut env, &method);
    let path = jstr(&mut env, &path);
    let body = if body.is_null() { None } else { Some(jstr(&mut env, &body)) };
    let response = crate::blocking::request(&method, &path, body.as_deref());
    out(&env, &response)
}

/// Trade the current token for one with the same scope and a fresh
/// expiry; returns the blocking facade's JSON envelope. The app persists
/// the returned token itself.
#[no_mangle]
pub extern "system" fn Java_com_example_bezellists_Bezel_nativeRefreshCapability(
    env: JNIEnv,
    _class: JClass,
    ttl_secs: jlong,
) -> jstring {
    let response = crate::blocking::refresh_capability(ttl_secs);
    out(&env, &response)
}

/// Open a change-feed subscription from `since`; `facet` may be null for
/// the whole feed. Returns the handle, or 0 on failure.
#[no_mangle]
pub extern "system" fn Java_com_example_bezellists_Bezel_nativeSubscribeChanges(
    mut env: JNIEnv,
    _class: JClass,
    since: jlong,
    facet: JString,
) -> jlong {
    let facet = if facet.is_null() { None } else { Some(jstr(&mut env, &facet)) };
    crate::blocking::subscribe_changes(since, facet.as_deref()).unwrap_or(0) as jlong
}

/// Block up to `timeout_ms` for the next change; returns the blocking
/// facade's JSON envelope. Call it from a background thread in a loop,
/// remembering each change's `seq` as the resume cursor.
#[no_mangle]
pub extern "system" fn Java_com_example_bezellists_Bezel_nativeNextChange(
    env: JNIEnv,
    _class: JClass,
    handle: jlong,
    timeout_ms: jlong,
) -> jstring {
    let response = crate::blocking::next_change(handle as u64, timeout_ms.max(0) as u64);
    out(&env, &response)
}

/// Close a subscription and its stream.
#[no_mangle]
pub extern "system" fn Java_com_example_bezellists_Bezel_nativeCloseSubscription(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
) {
    crate::blocking::close_subscription(handle as u64);
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if s.len() != 64 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}
