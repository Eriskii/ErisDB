//! Dial an ErisDB by endpoint id and issue one request — a reachability
//! probe: `ERISDB_SERVER=<id> ERISDB_TOKEN=<bz1…> cargo run --example dial`.

fn main() {
    let server = std::env::var("ERISDB_SERVER").expect("ERISDB_SERVER");
    let token = std::env::var("ERISDB_TOKEN").expect("ERISDB_TOKEN");
    if let Err(e) = erisdb_client::blocking::configure(&server, &token, "dial-probe", &[0u8; 32]) {
        eprintln!("configure failed: {e}");
        std::process::exit(1);
    }
    // `/v1/permissions` needs no permission, so this probes reachability
    // and nothing else — and prints what the token turned out to hold.
    println!("{}", erisdb_client::blocking::permissions());
}
