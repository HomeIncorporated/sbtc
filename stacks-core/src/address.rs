use std::fmt;

use bitcoin::blockdata::{opcodes::all::OP_CHECKMULTISIG, script::Builder};
use strum::{EnumIter, FromRepr};

use crate::{
    c32::{decode_address, encode_address},
    crypto::{
        hash::{Hash160, SHA256Hash, HASH160_LENGTH},
        PublicKey,
    },
    StacksError, StacksResult,
};

/// Supported stacks address versions
#[repr(u8)]
#[derive(FromRepr, EnumIter, PartialEq, Eq, Copy, Clone, Debug)]
pub enum AddressVersion {
    MainnetSingleSig = 22,
    MainnetMultiSig = 20,
    TestnetSingleSig = 26,
    TestnetMultiSig = 21,
}

impl TryFrom<u8> for AddressVersion {
    type Error = StacksError;

    fn try_from(value: u8) -> StacksResult<Self> {
        AddressVersion::from_repr(value).ok_or(StacksError::InvalidAddressVersion(value))
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum AddressHashMode {
    SerializeP2PKH,
    SerializeP2SH,
    SerializeP2WPKH,
    SerializeP2WSH,
}

#[derive(Debug, Clone)]
pub struct StacksAddress {
    version: AddressVersion,
    hash: Hash160,
}

impl StacksAddress {
    pub fn new(version: AddressVersion, hash: Hash160) -> Self {
        Self { version, hash }
    }

    pub fn version(&self) -> AddressVersion {
        self.version
    }

    pub fn hash(&self) -> &Hash160 {
        &self.hash
    }

    pub fn from_public_keys(
        version: AddressVersion,
        public_keys: &[PublicKey],
        signatures: usize,
        hash_mode: AddressHashMode,
    ) -> StacksResult<Self> {
        let public_key_count = public_keys.len();

        if public_key_count < signatures {
            return Err(StacksError::InvalidArguments(
                "Cannot require more signatures than public keys",
            ));
        }

        if matches!(
            hash_mode,
            AddressHashMode::SerializeP2PKH | AddressHashMode::SerializeP2WPKH
        ) {
            if public_key_count != 1 {
                return Err(StacksError::InvalidArguments(
                    "Cannot use more than one public key for this hash mode",
                ));
            }

            if signatures != 1 {
                return Err(StacksError::InvalidArguments(
                    "Cannot require more than one signature for this hash mode",
                ));
            }
        }

        let hash = match hash_mode {
            AddressHashMode::SerializeP2PKH => hash_p2pkh(&public_keys[0]),
            AddressHashMode::SerializeP2SH => hash_p2sh(signatures, public_keys),
            AddressHashMode::SerializeP2WPKH => hash_p2wpkh(&public_keys[0]),
            AddressHashMode::SerializeP2WSH => hash_p2wsh(signatures, public_keys),
        };

        Ok(Self::new(version, hash))
    }
}

impl TryFrom<&StacksAddress> for String {
    type Error = StacksError;

    fn try_from(address: &StacksAddress) -> Result<Self, Self::Error> {
        encode_address(address.version, address.hash.as_ref()).map_err(|err| err.into())
    }
}

impl TryFrom<&str> for StacksAddress {
    type Error = StacksError;

    fn try_from(address: &str) -> Result<Self, Self::Error> {
        let (version, hash_bytes) =
            decode_address(address).map_err::<StacksError, _>(|err| err.into())?;

        if hash_bytes.len() != HASH160_LENGTH {
            return Err(StacksError::InvalidArguments(
                "Invalid hash length for address",
            ));
        }

        let mut buffer = [0; HASH160_LENGTH];
        buffer.copy_from_slice(&hash_bytes);

        Ok(Self::new(version, buffer.into()))
    }
}

impl fmt::Display for StacksAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", String::try_from(self).unwrap())
    }
}

fn hash_p2pkh(key: &PublicKey) -> Hash160 {
    Hash160::new(key.serialize())
}

fn hash_p2sh(num_sigs: usize, pub_keys: &[PublicKey]) -> Hash160 {
    let mut builder = Builder::new();

    builder = builder.push_int(num_sigs as i64);

    for key in pub_keys {
        builder = builder.push_slice(&key.serialize());
    }

    builder = builder.push_int(pub_keys.len() as i64);
    builder = builder.push_opcode(OP_CHECKMULTISIG);

    let script = builder.into_script();
    let script_hash = Hash160::new(script.as_bytes());

    script_hash
}

fn hash_p2wpkh(key: &PublicKey) -> Hash160 {
    let key_hash_hasher = Hash160::new(key.serialize());
    let key_hash = key_hash_hasher.as_ref();
    let key_hash_len = key_hash.len();

    let mut buff = Vec::with_capacity(key_hash_len + 2);
    buff.push(0);
    buff.push(key_hash_len as u8);
    buff.extend_from_slice(key_hash);

    Hash160::new(&buff)
}

fn hash_p2wsh(num_sigs: usize, pub_keys: &[PublicKey]) -> Hash160 {
    let mut script = vec![];
    script.push(num_sigs as u8 + 80);

    for pub_key in pub_keys {
        let bytes = pub_key.serialize();

        script.push(bytes.len() as u8);
        script.extend_from_slice(&bytes);
    }

    script.push(pub_keys.len() as u8 + 80);
    script.push(174);

    let digest = SHA256Hash::new(&script);
    let digest_bytes = digest.as_ref();

    let mut buff = vec![];
    buff.push(0);
    buff.push(digest_bytes.len() as u8);
    buff.extend_from_slice(digest_bytes);

    Hash160::new(&buff)
}

#[cfg(test)]
mod tests {
    use super::*;

    /**
    Sample data computed with these commands on MacOS:

    ```
    CREDENTIALS=$(stx make_keychain)
    PUBLIC_KEY=$(echo $CREDENTIALS | jq -r .key_info.publicKey)
    EXPECTED_HASH=$(echo $PUBLIC_KEY \
        | xxd -r -p \
        | openssl dgst -sha256 -binary \
        | openssl dgst -ripemd160 -binary \
        | xxd -p)
    ```
    */
    #[test]
    fn should_correctly_hash_p2pkh() {
        let public_key_hex = "03556902f83defc6c63a7eb56a2d8ee4baee109f2126aac41e4f9e3a0835f34bc5";
        let expected_hash_hex = "d24206d58967c61b6b302eb14cd254a8ae7e761a";

        let pk = PublicKey::from_slice(&hex::decode(public_key_hex).unwrap()).unwrap();
        let hash_hex = hex::encode(hash_p2pkh(&pk).as_ref());

        assert_eq!(hash_hex, expected_hash_hex);
    }

    /// Data obtained from from blockstack_lib throwaway code
    #[test]
    fn should_correctly_hash_p2sh() {
        let pk_hex = "028cac21ac93bf697dc31da79e11aad8d285b2e2e81bcfc8de982179c6d468d339";
        let addr_hash = "fc1058076c56333d7d2d9fbb936aefa632c0e7a8";

        let pk = PublicKey::from_slice(&hex::decode(pk_hex).unwrap()).unwrap();
        let expected_hash: Hash160 = hex::decode(addr_hash)
            .unwrap()
            .as_slice()
            .try_into()
            .unwrap();

        assert_eq!(hash_p2sh(1, &[pk]).as_ref(), expected_hash.as_ref());
    }

    /// Data obtained from from blockstack_lib throwaway code
    #[test]
    fn should_correctly_hash_p2sh_2_keys() {
        let pk1_hex = "0325a1b9799db9852ee1c99280b20695b1889eff7ec0352d634912818d02f91f84";
        let pk2_hex = "0279d7abd36d41d51e225efbbc8376a257051cecdf8b47eaffeb49b77547bc3bff";
        let addr_hash = "073503b6e6ef916e4ab40f31abc83217c271d917";

        let pk1 = PublicKey::from_slice(&hex::decode(pk1_hex).unwrap()).unwrap();
        let pk2 = PublicKey::from_slice(&hex::decode(pk2_hex).unwrap()).unwrap();
        let expected_hash: Hash160 = hex::decode(addr_hash)
            .unwrap()
            .as_slice()
            .try_into()
            .unwrap();

        assert_eq!(hash_p2sh(2, &[pk1, pk2]).as_ref(), expected_hash.as_ref());
    }

    /// Data obtained from from blockstack_lib throwaway code
    #[test]
    fn should_correctly_hash_p2wsh() {
        let pk_hex = "027cf49417052b14d73c3da78ec3c0c859380b19a4971fd8c63ded9037455dd84c";
        let addr_hash = "599623097df78a0e962108bfb0f1f78ef1d15f57";

        let pk = PublicKey::from_slice(&hex::decode(pk_hex).unwrap()).unwrap();
        let expected_hash: Hash160 = hex::decode(addr_hash)
            .unwrap()
            .as_slice()
            .try_into()
            .unwrap();

        assert_eq!(hash_p2wsh(1, &[pk]).as_ref(), expected_hash.as_ref());
    }

    /// Data obtained from from blockstack_lib throwaway code
    #[test]
    fn should_correctly_hash_p2wsh_2_key() {
        let pk1_hex = "037c6e4c27b3d39ab73c2cd2fdd2ea34cec3d9b6881a2a4a17e42fcafb6b64c3aa";
        let pk2_hex = "03a544a1d3fb4238d5841647100c53e371a1d72f027857899256f0c754cf266491";
        let addr_hash = "d5f3ddac2358f61088d951aead20c270a045d46d";

        let pk1 = PublicKey::from_slice(&hex::decode(pk1_hex).unwrap()).unwrap();
        let pk2 = PublicKey::from_slice(&hex::decode(pk2_hex).unwrap()).unwrap();
        let expected_hash: Hash160 = hex::decode(addr_hash)
            .unwrap()
            .as_slice()
            .try_into()
            .unwrap();

        assert_eq!(hash_p2wsh(2, &[pk1, pk2]).as_ref(), expected_hash.as_ref());
    }

    /// Data obtained from from blockstack_lib throwaway code
    #[test]
    fn should_correctly_hash_p2wpkh() {
        let pk_hex = "03528351fc1494c66b67e0857fd571e1de37985dd0cae987dbe71c47d2bc7a7712";
        let addr_hash = "3bb7c80b72757b4bc94bd3cb09171500fb72b4ac";

        let pk = PublicKey::from_slice(&hex::decode(pk_hex).unwrap()).unwrap();
        let expected_hash: Hash160 = hex::decode(addr_hash)
            .unwrap()
            .as_slice()
            .try_into()
            .unwrap();

        assert_eq!(hash_p2wpkh(&pk).as_ref(), expected_hash.as_ref());
    }

    /// Data generated with `stx make_keychain`
    #[test]
    fn should_create_correct_address_from_public_key() {
        let public_key = "02e2ce887c1f1654936fbb7d4036749da5e7b9b64af406e1f3535c8f4336de1c6e";
        let expected_address = "SPR4FMGJCD78NF4FRGPM621CW1KHNFEG0HSRDSPK";

        let addr = StacksAddress::from_public_keys(
            AddressVersion::MainnetSingleSig,
            &[PublicKey::from_slice(&hex::decode(public_key).unwrap()).unwrap()],
            1,
            AddressHashMode::SerializeP2PKH,
        )
        .unwrap();

        assert_eq!(String::try_from(&addr).unwrap(), expected_address);
    }

    /// Data generated with `stx make_keychain`
    #[test]
    fn should_create_correct_address_from_c32_encoded_string() {
        let addr = "SPR4FMGJCD78NF4FRGPM621CW1KHNFEG0HSRDSPK";
        let public_key_hex = "02e2ce887c1f1654936fbb7d4036749da5e7b9b64af406e1f3535c8f4336de1c6e";
        let expected_hash =
            hash_p2pkh(&PublicKey::from_slice(&hex::decode(public_key_hex).unwrap()).unwrap());

        let addr = StacksAddress::try_from(addr).unwrap();

        assert_eq!(addr.hash(), &expected_hash);
    }
}
