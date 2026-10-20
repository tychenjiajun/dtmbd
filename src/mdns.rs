//! L6 — discovery: advertise `http://<hostname>.local:8080/playlist.m3u` over
//! mDNS so the TV does not care about DHCP addresses.
//!
//! Best effort: if mDNS fails (no multicast route, firewall) the service still
//! runs, the playlist URL just falls back to the LAN IP.

use mdns_sd::{ServiceDaemon, ServiceInfo};

const SERVICE: &str = "_http._tcp.local.";

/// Local IPv4 used to reach the internet — the LAN address in practice.
pub fn local_ip() -> String {
    std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            s.connect("223.5.5.5:53")?; // no packets are sent
            s.local_addr()
        })
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "127.0.0.1".to_string())
}

pub fn advertise(hostname: &str, port: u16, ip: &str) {
    match advertise_inner(hostname, port, ip) {
        Ok(()) => println!("mDNS: http://{hostname}.local:{port}/playlist.m3u"),
        Err(e) => println!("mDNS: not advertised ({e}); use http://{ip}:{port}/playlist.m3u"),
    }
}

fn advertise_inner(hostname: &str, port: u16, ip: &str) -> Result<(), Box<dyn std::error::Error>> {
    let daemon = ServiceDaemon::new()?;
    let host = format!("{hostname}.local.");
    let info = ServiceInfo::new(
        SERVICE,
        "DTMB IPTV",
        &host,
        ip,
        port,
        &[("path", "/playlist.m3u")][..],
    )?;
    daemon.register(info)?;
    // the daemon keeps running in its own thread; leaking it is intentional
    std::mem::forget(daemon);
    Ok(())
}
