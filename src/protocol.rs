//! Protocole TCP minimaliste.
//!
//! Source -> viewer : `[u32 LE longueur][u8 type][données]`
//! Viewer -> source : `[u8 type][u8 longueur][données]` (requêtes et entrées souris/clavier)

use std::io::{self, Read, Write};

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

pub const MAGIC: &[u8; 8] = b"PIPPBP02";

/// Paquet vidéo : un frame H.264 (Annex B) complet.
pub const MSG_VIDEO: u8 = 1;
/// Un autre viewer a pris la place de celui-ci : il doit se fermer sans se reconnecter.
pub const MSG_REPLACED: u8 = 2;
/// Premier paquet après MAGIC : `[u8 contrôle autorisé][16 octets aléatoires]`.
pub const MSG_HELLO: u8 = 3;
/// Résultat de l'authentification : `[u8 1 = accepté, 0 = refusé]`.
pub const MSG_AUTH: u8 = 4;

/// Requête du viewer : envoyer une image clé (après une erreur de décodage).
pub const REQ_KEYFRAME: u8 = 1;
/// Preuve du code de contrôle : HMAC-SHA256(code, nombre aléatoire du HELLO), 32 octets.
pub const REQ_AUTH: u8 = 2;
/// Position absolue de la souris sur l'écran partagé : `[u16 x][u16 y]`, 0..=65535.
pub const IN_MOUSE_MOVE: u8 = 10;
/// Bouton souris : `[u8 bouton 0=gauche 1=droit 2=milieu 3=X1 4=X2][u8 1=enfoncé]`.
pub const IN_MOUSE_BUTTON: u8 = 11;
/// Molette : `[i16 vertical][i16 horizontal]`, en unités Windows (120 = un cran).
pub const IN_WHEEL: u8 = 12;
/// Touche : `[u16 scancode (0xE0xx = étendu)][u8 1=enfoncée]`.
pub const IN_KEY: u8 = 13;

pub const NONCE_LEN: usize = 16;

/// Taille maximale acceptée pour un paquet (protection contre les données corrompues).
const MAX_PACKET: usize = 64 * 1024 * 1024;

pub fn write_packet(w: &mut impl Write, kind: u8, payload: &[u8]) -> io::Result<()> {
    let mut header = [0u8; 5];
    header[..4].copy_from_slice(&(payload.len() as u32 + 1).to_le_bytes());
    header[4] = kind;
    w.write_all(&header)?;
    w.write_all(payload)?;
    w.flush()
}

/// Lit un paquet dans `buf` (réutilisé pour éviter les allocations) et renvoie son type.
pub fn read_packet(r: &mut impl Read, buf: &mut Vec<u8>) -> io::Result<u8> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len == 0 || len > MAX_PACKET {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "taille de paquet invalide"));
    }
    let mut kind = [0u8; 1];
    r.read_exact(&mut kind)?;
    buf.resize(len - 1, 0);
    r.read_exact(buf)?;
    Ok(kind[0])
}

/// Message court du viewer vers la source (un seul `write` pour TCP_NODELAY).
pub fn write_request(w: &mut impl Write, kind: u8, payload: &[u8]) -> io::Result<()> {
    let mut msg = Vec::with_capacity(2 + payload.len());
    msg.push(kind);
    msg.push(payload.len() as u8);
    msg.extend_from_slice(payload);
    w.write_all(&msg)
}

pub fn read_request(r: &mut impl Read) -> io::Result<(u8, Vec<u8>)> {
    let mut head = [0u8; 2];
    r.read_exact(&mut head)?;
    let mut payload = vec![0u8; head[1] as usize];
    r.read_exact(&mut payload)?;
    Ok((head[0], payload))
}

/// Preuve que l'on connaît le code, sans jamais l'envoyer : HMAC-SHA256(code, nonce).
pub fn auth_proof(code: &str, nonce: &[u8]) -> Vec<u8> {
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(code.trim().as_bytes()).expect("HMAC accepte toute clé");
    mac.update(nonce);
    mac.finalize().into_bytes().to_vec()
}

/// Vérification en temps constant de la preuve envoyée par le viewer.
pub fn check_auth(code: &str, nonce: &[u8], proof: &[u8]) -> bool {
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(code.trim().as_bytes()).expect("HMAC accepte toute clé");
    mac.update(nonce);
    mac.verify_slice(proof).is_ok()
}

/// Code de contrôle aléatoire à 6 chiffres.
pub fn new_code() -> String {
    let n = getrandom::u32().unwrap_or(0) % 1_000_000;
    format!("{n:06}")
}

pub fn new_nonce() -> [u8; NONCE_LEN] {
    let mut n = [0u8; NONCE_LEN];
    let _ = getrandom::fill(&mut n);
    n
}
