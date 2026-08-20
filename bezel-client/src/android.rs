//! JNI surface for the Android lists app. Thin by design: strings in,
//! JSON strings out, all real work in [`crate::blocking`].

use jni::objects::{JClass, JString};
use jni::sys::jstring;
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

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if s.len() != 64 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}
