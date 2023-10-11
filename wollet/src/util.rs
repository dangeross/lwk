use crate::elements::bitcoin::secp256k1::PublicKey;
use crate::elements::hex::{FromHex, ToHex};
use crate::elements::Script;
use crate::error::Error;
use crate::secp256k1;
use elements_miniscript::{ConfidentialDescriptor, DescriptorPublicKey};
use rand::thread_rng;
use serde::Deserialize;

pub static EC: once_cell::sync::Lazy<secp256k1::Secp256k1<secp256k1::All>> =
    once_cell::sync::Lazy::new(|| {
        let mut ctx = secp256k1::Secp256k1::new();
        let mut rng = thread_rng();
        ctx.randomize(&mut rng);
        ctx
    });

pub fn ciborium_to_vec<T>(value: &T) -> Result<Vec<u8>, ciborium::ser::Error<std::io::Error>>
where
    T: serde::ser::Serialize,
{
    let mut v = Vec::new();
    ciborium::ser::into_writer(value, &mut v)?;
    Ok(v)
}

/// Deserializes a hex string to a `Vec<u8>`.
pub fn serde_from_hex<'de, D>(deserializer: D) -> std::result::Result<Vec<u8>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    String::deserialize(deserializer).and_then(|string| {
        Vec::<u8>::from_hex(&string).map_err(|err| Error::custom(err.to_string()))
    })
}

/// Serializes a Vec<u8> into a hex string.
pub fn serde_to_hex<T, S>(buffer: &T, serializer: S) -> std::result::Result<S::Ok, S::Error>
where
    T: AsRef<[u8]>,
    S: serde::Serializer,
{
    serializer.serialize_str(&buffer.as_ref().to_hex())
}

pub fn verify_pubkey(pubkey: &[u8]) -> Result<(), Error> {
    PublicKey::from_slice(pubkey)?;
    Ok(())
}

pub fn derive_script_pubkey(
    descriptor: &ConfidentialDescriptor<DescriptorPublicKey>,
    index: u32,
) -> Result<Script, Error> {
    Ok(descriptor
        .descriptor
        .at_derivation_index(index)?
        .script_pubkey())
}
