use tls_codec::{TlsDeserialize, TlsSerialize, TlsSize};

use super::{Deserialize, Serialize};
use crate::prelude::HpkePublicKey;

/// ```c
/// // draft-ietf-mls-protocol-16
/// struct {
///     HPKEPublicKey external_pub;
/// } ExternalPub;
/// ```
#[derive(
    PartialEq, Eq, Clone, Debug, Serialize, Deserialize, TlsSerialize, TlsDeserialize, TlsSize,
)]
pub struct ExternalPubExtension {
    external_pub: HpkePublicKey,
}

impl ExternalPubExtension {
    /// Create a new `external_pub` extension.
    pub fn new(external_pub: HpkePublicKey) -> Self {
        Self { external_pub }
    }

    /// Get a reference to the HPKE public key.
    pub fn external_pub(&self) -> &HpkePublicKey {
        &self.external_pub
    }
}

#[cfg(test)]
mod test {
    use openmls_rust_crypto::OpenMlsRustCrypto;
    use openmls_traits::types::{Ciphersuite, SignatureScheme};
    use tls_codec::{Deserialize, Serialize};

    use super::*;
    use crate::{
        credentials::{CredentialBundle, CredentialType},
        key_packages::KeyPackageBundle,
    };

    #[test]
    fn test_serialize_deserialize() {
        let tests = {
            let backend = OpenMlsRustCrypto::default();

            let mut external_pub_extensions = Vec::new();

            for _ in 0..8 {
                let hpke_public_key = {
                    let credential_bundle = CredentialBundle::new(
                        b"Alice".to_vec(),
                        CredentialType::Basic,
                        SignatureScheme::ED25519,
                        &backend,
                    )
                    .expect("Creation of credential bundle failed.");

                    let kpb = KeyPackageBundle::new(
                        &[Ciphersuite::MLS_128_DHKEMX25519_CHACHA20POLY1305_SHA256_Ed25519],
                        &credential_bundle,
                        &backend,
                        vec![],
                    )
                    .unwrap();

                    kpb.key_package().hpke_init_key().clone()
                };

                external_pub_extensions.push(ExternalPubExtension::new(hpke_public_key));
            }

            external_pub_extensions
        };

        for expected in tests {
            let serialized = expected.tls_serialize_detached().unwrap();
            let serialized = &mut serialized.as_slice();

            let got = ExternalPubExtension::tls_deserialize(serialized).unwrap();

            assert!(serialized.is_empty());
            assert_eq!(expected, got);
        }
    }
}
