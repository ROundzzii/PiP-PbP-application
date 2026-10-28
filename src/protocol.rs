//! Protocole TCP minimaliste.
//!
//! Source -> viewer : `[u32 LE longueur][u8 type][données]`
//! Viewer -> source : un octet par requête.

use std::io::{self, Read, Write};

pub const MAGIC: &[u8; 8] = b"PIPPBP01";

/// Paquet vidéo : un frame H.264 (Annex B) complet.
pub const MSG_VIDEO: u8 = 1;
/// Un autre viewer a pris la place de celui-ci : il doit se fermer sans se reconnecter.
pub const MSG_REPLACED: u8 = 2;

/// Requête du viewer : envoyer une image clé (après une erreur de décodage).
pub const REQ_KEYFRAME: u8 = 1;

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
