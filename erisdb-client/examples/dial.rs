//! Dial an ErisDB as an app and issue one request — a reachability probe:
//! `ERISDB_SERVER=<endpoint id or address JSON> ERISDB_KEY=<64 hex> cargo run --example dial`.

fn main() {
    let server = std::env::var("ERISDB_SERVER").expect("ERISDB_SERVER");
    let key = std::env::var("ERISDB_KEY").expect("ERISDB_KEY");
    let Some(key) = erisdb_client::blocking::decode_key_hex(&key) else {
        eprintln!("ERISDB_KEY is 64 hex characters");
        std::process::exit(1);
    };
    if let Err(e) = erisdb_client::blocking::dial(&server, "dial-probe", &key) {
        eprintln!("dial failed: {e}");
        std::process::exit(1);
    }
    // `/v1/permissions` needs no permission, so any answer proves ErisDB
    // reachable: what the key holds when a device enrolled it, 401 when not.
    println!("{}", erisdb_client::blocking::permissions());
}
