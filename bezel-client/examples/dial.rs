//! Dial a bezel by endpoint id and issue one request — a reachability
//! probe: `BEZEL_SERVER=<id> BEZEL_TOKEN=<bz1…> cargo run --example dial`.

fn main() {
    let server = std::env::var("BEZEL_SERVER").expect("BEZEL_SERVER");
    let token = std::env::var("BEZEL_TOKEN").expect("BEZEL_TOKEN");
    if let Err(e) = bezel_client::blocking::configure(&server, &token, "dial-probe", &[0u8; 32]) {
        eprintln!("configure failed: {e}");
        std::process::exit(1);
    }
    println!("{}", bezel_client::blocking::request("GET", "/v1/items?facet=facet&limit=5", None));
}
