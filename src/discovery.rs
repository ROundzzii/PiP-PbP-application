//! Découverte automatique sur le réseau local : pas besoin de connaître l'IP de la source.
//!
//! Le viewer envoie une question en broadcast UDP sur chaque carte réseau ; chaque
//! source répond directement. Seule la source écoute (même port que le TCP), donc une
//! seule autorisation pare-feu suffit, sur la source.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

const QUERY: &[u8] = b"PIPPBP?";
const REPLY: &[u8] = b"PIPPBP!";

/// Adresses IPv4 locales utilisables (hors boucle locale), avec leur adresse de broadcast.
pub fn local_ipv4() -> Vec<(String, Ipv4Addr, Option<Ipv4Addr>)> {
    let Ok(ifaces) = if_addrs::get_if_addrs() else { return Vec::new() };
    ifaces
        .into_iter()
        .filter(|i| i.is_oper_up() && !i.is_loopback())
        .filter_map(|i| match i.addr {
            if_addrs::IfAddr::V4(a) if !a.ip.is_link_local() => Some((i.name, a.ip, a.broadcast)),
            _ => None,
        })
        .collect()
}

/// Côté source : répond aux viewers qui cherchent une source (thread en arrière-plan).
pub fn spawn_responder(port: u16) {
    let socket = match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Découverte automatique indisponible (UDP {port}) : {e}");
            return;
        }
    };
    let name = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "PC".into());
    std::thread::spawn(move || {
        let mut buf = [0u8; 64];
        loop {
            let Ok((n, from)) = socket.recv_from(&mut buf) else { continue };
            if &buf[..n] == QUERY {
                let mut reply = REPLY.to_vec();
                reply.extend_from_slice(name.as_bytes());
                let _ = socket.send_to(&reply, from);
            }
        }
    });
}

/// Côté viewer : cherche une source pendant `timeout`. Renvoie son adresse et son nom.
pub fn find_source(port: u16, timeout: Duration) -> Option<(IpAddr, String)> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.set_broadcast(true).ok()?;
    socket.set_read_timeout(Some(Duration::from_millis(250))).ok()?;

    let mut targets: Vec<SocketAddr> = vec![(Ipv4Addr::BROADCAST, port).into()];
    for (_, _, broadcast) in local_ipv4() {
        if let Some(b) = broadcast {
            targets.push((b, port).into());
        }
    }

    let deadline = Instant::now() + timeout;
    let mut buf = [0u8; 128];
    while Instant::now() < deadline {
        for t in &targets {
            let _ = socket.send_to(QUERY, t);
        }
        let round = Instant::now() + Duration::from_millis(750);
        while Instant::now() < round {
            if let Ok((n, from)) = socket.recv_from(&mut buf)
                && buf[..n].starts_with(REPLY)
            {
                let name = String::from_utf8_lossy(&buf[REPLY.len()..n]).into_owned();
                return Some((from.ip(), name));
            }
        }
    }
    None
}
