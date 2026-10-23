//! This node's TLS identity: a self-signed certificate, generated once and reused.
//!
//! Self-signed and pinned rather than CA-issued, because there is no CA on a home LAN and asking for
//! one would make the product unusable. Trust comes from pairing instead: both machines prove
//! knowledge of the pairing code, each records the other's thumbprint, and afterwards that
//! thumbprint alone is the identity.
//!
//! The private key is stored encrypted with DPAPI under the account the service runs as. As
//! LocalSystem, that means only code already running as SYSTEM can recover it -- the same audience a
//! machine key store admits -- while the certificate beside it is public and stored in the clear.

use std::fs;
use std::io;
use std::path::Path;

use rcgen::{CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use uuid::Uuid;

const CERTIFICATE_FILE: &str = "node.crt";
const KEY_FILE: &str = "node.key";

/// The identity a peer is pinned by: SHA-256 of the whole certificate, as uppercase hex.
pub fn thumbprint_of(certificate_der: &[u8]) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, certificate_der);
    digest.as_ref().iter().map(|byte| format!("{byte:02X}")).collect()
}

pub struct NodeIdentity {
    certificate: Vec<u8>,
    private_key: Vec<u8>,
    thumbprint: String,
}

impl NodeIdentity {
    /// Creates a fresh self-signed certificate, held only in memory.
    ///
    /// ECDSA P-256: TLS 1.3 handshakes with it are an order of magnitude cheaper than with RSA, and
    /// generating one is instant, so a machine pays nothing noticeable on first run.
    pub fn generate(common_name: &str) -> io::Result<Self> {
        let key_pair = KeyPair::generate().map_err(io::Error::other)?;

        let mut params = CertificateParams::new(vec!["VirtualKmSwitch".to_owned()]).map_err(io::Error::other)?;
        params.distinguished_name = DistinguishedName::new();
        params.distinguished_name.push(DnType::CommonName, common_name);
        params.is_ca = IsCa::ExplicitNoCa;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];

        // Both roles, because every peer both dials and accepts: the mesh has no clients or
        // servers, only peers.
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth, ExtendedKeyUsagePurpose::ClientAuth];

        // Validity is left at rcgen's very wide default. Nothing checks it -- the pin is the trust
        // decision -- and a narrow window would only add a way for a peer with a wrong clock to be
        // refused, or for a mesh to stop working the day a certificate quietly expires.
        let certificate = params.self_signed(&key_pair).map_err(io::Error::other)?;

        Ok(Self::from_parts(certificate.der().to_vec(), key_pair.serialize_der()))
    }

    fn from_parts(certificate: Vec<u8>, private_key: Vec<u8>) -> Self {
        let thumbprint = thumbprint_of(&certificate);
        Self { certificate, private_key, thumbprint }
    }

    /// Loads this node's identity from `directory`, creating and saving one on first use.
    pub fn load_or_create(directory: &Path, node_id: Uuid) -> io::Result<Self> {
        let certificate_path = directory.join(CERTIFICATE_FILE);
        let key_path = directory.join(KEY_FILE);

        if certificate_path.exists() && key_path.exists() {
            match Self::load(&certificate_path, &key_path) {
                Ok(identity) => return Ok(identity),
                Err(error) => {
                    // Typically a key protected under a different account -- the service run once
                    // with --console as a user, say. A fresh identity means every paired machine
                    // must pair again, which is loud but recoverable; refusing to start is neither.
                    log::error!(
                        "the stored node identity could not be used ({error}); generating a new one, so machines \
                         paired with this one will have to pair again"
                    );
                }
            }
        }

        let identity = Self::generate(&format!("VirtualKmSwitch-{}", node_id.simple()))?;
        identity.save(&certificate_path, &key_path)?;
        Ok(identity)
    }

    fn load(certificate_path: &Path, key_path: &Path) -> io::Result<Self> {
        let certificate = fs::read(certificate_path)?;
        let private_key = vkms_win::dpapi::unprotect(&fs::read(key_path)?)?;

        // A key the TLS stack cannot load is worse than none: it would be presented, then fail
        // every handshake with an opaque error.
        crate::tls::provider()
            .key_provider
            .load_private_key(PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(private_key.clone())))
            .map_err(io::Error::other)?;

        Ok(Self::from_parts(certificate, private_key))
    }

    fn save(&self, certificate_path: &Path, key_path: &Path) -> io::Result<()> {
        if let Some(directory) = certificate_path.parent() {
            fs::create_dir_all(directory)?;
        }

        let sealed = vkms_win::dpapi::protect(&self.private_key, "Virtual KM Switch node key")?;

        // Key first: a certificate without its key is useless, whereas a key without its
        // certificate is simply regenerated next time.
        write_replacing(key_path, &sealed)?;
        write_replacing(certificate_path, &self.certificate)
    }

    pub fn thumbprint(&self) -> &str {
        &self.thumbprint
    }

    pub fn certificate(&self) -> CertificateDer<'static> {
        CertificateDer::from(self.certificate.clone())
    }

    pub fn private_key(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.private_key.clone()))
    }
}

fn write_replacing(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, bytes)?;
    fs::rename(&temporary, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_thumbprint_is_the_sha256_of_the_certificate_in_uppercase_hex() {
        let identity = NodeIdentity::generate("CN").unwrap();
        assert_eq!(64, identity.thumbprint().len());
        assert!(identity.thumbprint().chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_lowercase()));
        assert_eq!(identity.thumbprint(), thumbprint_of(&identity.certificate()));
    }

    #[test]
    fn an_identity_survives_a_restart() {
        let directory = std::env::temp_dir().join(format!("vkms-identity-{}", Uuid::new_v4().simple()));
        let node = Uuid::new_v4();

        let first = NodeIdentity::load_or_create(&directory, node).unwrap();
        let second = NodeIdentity::load_or_create(&directory, node).unwrap();

        assert_eq!(first.thumbprint(), second.thumbprint());

        // The key is not on disk in the clear.
        let stored = fs::read(directory.join(KEY_FILE)).unwrap();
        assert!(!stored.windows(16).any(|window| first.private_key.windows(16).any(|key| key == window)));

        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn a_damaged_key_is_replaced_rather_than_presented() {
        let directory = std::env::temp_dir().join(format!("vkms-identity-{}", Uuid::new_v4().simple()));
        let node = Uuid::new_v4();

        let first = NodeIdentity::load_or_create(&directory, node).unwrap();
        fs::write(directory.join(KEY_FILE), b"not a key").unwrap();

        let second = NodeIdentity::load_or_create(&directory, node).unwrap();
        assert_ne!(first.thumbprint(), second.thumbprint());

        let _ = fs::remove_dir_all(directory);
    }
}
