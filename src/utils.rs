use dns_lookup::{lookup_addr, lookup_host};
use hbb_common::{bail, ResultType};
use sodiumoxide::crypto::sign;
use std::{
    env,
    net::{IpAddr, TcpStream},
    path::PathBuf,
    process, str,
};

fn print_help() {
    println!(
        "Usage:
    rustdesk-utils [command]\n
Available Commands:
    genkeypair                                   Generate a new keypair
    validatekeypair [public key] [secret key]    Validate an existing keypair
    doctor [rustdesk-server]                     Check for server connection problems
    geo-update [--out FILE] [--loop] [--force] [--base-url URL]
                                                 Download DB-IP's monthly GeoIP database for
                                                 hbbs (GEO_DB), and keep it current with --loop"
    );
    process::exit(0x0001);
}

fn error_then_help(msg: &str) {
    println!("ERROR: {msg}\n");
    print_help();
}

fn gen_keypair() {
    let (pk, sk) = sign::gen_keypair();
    let public_key = base64::encode(pk);
    let secret_key = base64::encode(sk);
    println!("Public Key:  {public_key}");
    println!("Secret Key:  {secret_key}");
}

fn validate_keypair(pk: &str, sk: &str) -> ResultType<()> {
    let sk1 = base64::decode(sk);
    if sk1.is_err() {
        bail!("Invalid secret key");
    }
    let sk1 = sk1.unwrap();

    let secret_key = sign::SecretKey::from_slice(sk1.as_slice());
    if secret_key.is_none() {
        bail!("Invalid Secret key");
    }
    let secret_key = secret_key.unwrap();

    let pk1 = base64::decode(pk);
    if pk1.is_err() {
        bail!("Invalid public key");
    }
    let pk1 = pk1.unwrap();

    let public_key = sign::PublicKey::from_slice(pk1.as_slice());
    if public_key.is_none() {
        bail!("Invalid Public key");
    }
    let public_key = public_key.unwrap();

    let random_data_to_test = b"This is meh.";
    let signed_data = sign::sign(random_data_to_test, &secret_key);
    let verified_data = sign::verify(&signed_data, &public_key);
    if verified_data.is_err() {
        bail!("Key pair is INVALID");
    }
    let verified_data = verified_data.unwrap();

    if random_data_to_test != &verified_data[..] {
        bail!("Key pair is INVALID");
    }

    Ok(())
}

fn doctor_tcp(address: std::net::IpAddr, port: &str, desc: &str) {
    let start = std::time::Instant::now();
    let conn = format!("{address}:{port}");
    if let Ok(_stream) = TcpStream::connect(conn.as_str()) {
        let elapsed = std::time::Instant::now().duration_since(start);
        println!(
            "TCP Port {} ({}): OK in {} ms",
            port,
            desc,
            elapsed.as_millis()
        );
    } else {
        println!("TCP Port {port} ({desc}): ERROR");
    }
}

fn doctor_ip(server_ip_address: std::net::IpAddr, server_address: Option<&str>) {
    println!("\nChecking IP address: {server_ip_address}");
    println!("Is IPV4: {}", server_ip_address.is_ipv4());
    println!("Is IPV6: {}", server_ip_address.is_ipv6());

    // reverse dns lookup
    // TODO: (check) doesn't seem to do reverse lookup on OSX...
    let reverse = lookup_addr(&server_ip_address).unwrap();
    if let Some(server_address) = server_address {
        if reverse == server_address {
            println!("Reverse DNS lookup: '{reverse}' MATCHES server address");
        } else {
            println!(
                "Reverse DNS lookup: '{reverse}' DOESN'T MATCH server address '{server_address}'"
            );
        }
    }

    // TODO: ICMP ping?

    // port check TCP (UDP is hard to check)
    doctor_tcp(server_ip_address, "21114", "API");
    doctor_tcp(server_ip_address, "21115", "hbbs extra port for nat test");
    doctor_tcp(server_ip_address, "21116", "hbbs");
    doctor_tcp(server_ip_address, "21117", "hbbr tcp");
    doctor_tcp(server_ip_address, "21118", "hbbs websocket");
    doctor_tcp(server_ip_address, "21119", "hbbr websocket");

    // TODO: key check
}

fn doctor(server_address_unclean: &str) {
    let server_address3 = server_address_unclean.trim();
    let server_address2 = server_address3.to_lowercase();
    let server_address = server_address2.as_str();
    println!("Checking server:  {server_address}\n");
    if let Ok(server_ipaddr) = server_address.parse::<IpAddr>() {
        // user requested an ip address
        doctor_ip(server_ipaddr, None);
    } else {
        // the passed string is not an ip address
        let ips: Vec<std::net::IpAddr> = lookup_host(server_address).unwrap();
        println!("Found {} IP addresses: ", ips.len());

        ips.iter().for_each(|ip| println!(" - {ip}"));

        ips.iter()
            .for_each(|ip| doctor_ip(*ip, Some(server_address)));
    }
}

/// `geo-update`: fetch the GeoIP database once, or with `--loop` every day (it only
/// downloads when a new month's file is published, about once a month).
fn geo_update(args: &[String]) {
    use hbbs::geo_update::{update, Options, Outcome, DEFAULT_BASE_URL};
    use std::{thread::sleep, time::Duration};

    let (mut out, mut base, mut force, mut looping) = (
        env::var("GEO_DB").ok().filter(|v| !v.is_empty()),
        env::var("GEO_UPDATE_BASE_URL")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_owned()),
        false,
        false,
    );
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--out" => out = it.next().cloned(),
            "--base-url" => base = it.next().cloned().unwrap_or(base),
            "--force" => force = true,
            "--loop" => looping = true,
            other => error_then_help(&format!("unknown option {other}")),
        }
    }
    let out = PathBuf::from(out.unwrap_or_else(|| "geo.mmdb".to_owned()));
    let log = |msg: String| {
        println!(
            "[{}] geo-update: {msg}",
            chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
        );
    };
    loop {
        let now = {
            use chrono::Datelike;
            let d = chrono::Utc::now();
            (d.year(), d.month())
        };
        let mut o = Options::new(out.clone(), base.clone(), now);
        o.force = force;
        // A forced refresh is a one-off, not something to repeat every day.
        force = false;
        let failed = match update(&o) {
            Ok(Outcome::Updated(m)) => {
                log(format!(
                    "installed the {m} database at {} (data by DB-IP.com, CC BY 4.0)",
                    out.display()
                ));
                false
            }
            Ok(Outcome::UpToDate(m)) => {
                if !looping {
                    log(format!(
                        "already have the {m} database, nothing newer is published"
                    ));
                }
                false
            }
            Err(e) => {
                log(format!("failed, keeping the current file: {e}"));
                if !looping {
                    process::exit(0x0001);
                }
                true
            }
        };
        if !looping {
            return;
        }
        // A day between checks; after a failure, six hours.
        sleep(Duration::from_secs(if failed {
            6 * 3600
        } else {
            24 * 3600
        }));
    }
}

fn main() {
    let args: Vec<_> = env::args().collect();
    if args.len() <= 1 {
        print_help();
    }

    let command = args[1].to_lowercase();
    match command.as_str() {
        "genkeypair" => gen_keypair(),
        "validatekeypair" => {
            if args.len() <= 3 {
                error_then_help("You must supply both the public and the secret key");
            }
            let res = validate_keypair(args[2].as_str(), args[3].as_str());
            if let Err(e) = res {
                println!("{e}");
                process::exit(0x0001);
            }
            println!("Key pair is VALID");
        }
        "geo-update" => geo_update(&args[2..]),
        "doctor" => {
            if args.len() <= 2 {
                error_then_help("You must supply the rustdesk-server address");
            }
            doctor(args[2].as_str());
        }
        _ => print_help(),
    }
}
