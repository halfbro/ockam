use arrayref::array_ref;
use ockam_core::compat::sync::Arc;
use ockam_core::compat::vec::Vec;
use ockam_core::errcode::{Kind, Origin};
use ockam_core::{Error, Result};
use ockam_vault::constants::{AES256_SECRET_LENGTH_USIZE, X25519_PUBLIC_LENGTH_USIZE};
use ockam_vault::SecretType::X25519;
use ockam_vault::{KeyId, PublicKey, Secret, SecretAttributes, SecureChannelVault};
use sha2::{Digest, Sha256};
use Status::*;

use crate::secure_channel::handshake::error::XXError;
use crate::secure_channel::handshake::handshake_state_machine::{HandshakeKeys, Status};
use crate::secure_channel::Role;

/// The number of bytes in a SHA256 digest
pub const SHA256_SIZE_U32: u32 = 32;
/// The number of bytes in a SHA256 digest
pub const SHA256_SIZE_USIZE: usize = 32;
/// The number of bytes in AES-GCM tag
pub const AES_GCM_TAGSIZE_USIZE: usize = 16;

/// Implementation of a Handshake for the noise protocol
/// The first members are used in the implementation of some of the protocol steps, for example to
/// encrypt messages
/// The variables used in the protocol itself: s, e, rs, re,... are handled in `HandshakeState`
pub(super) struct Handshake {
    vault: Arc<dyn SecureChannelVault>,
    pub(super) state: HandshakeState,
}

/// Top-level functions used in the initiator and responder state machines
/// Each function makes mutable copy of the state to modify it in order to make the code more compact
/// and avoid self.state.xxx = ...
impl Handshake {
    /// Initialize the handshake variables
    pub(super) async fn initialize(&mut self) -> Result<()> {
        let mut state = self.state.clone();
        state.h = *Self::protocol_name();
        state.k = Some(
            self.import_k_secret(vec![0u8; AES256_SECRET_LENGTH_USIZE])
                .await?,
        );
        state.ck = Some(
            self.import_ck_secret(Self::protocol_name().to_vec())
                .await?,
        );

        state.h = HandshakeState::sha256(&state.h);
        self.state = state;
        Ok(())
    }

    /// Encode the first message, sent from the initiator to the responder
    pub(super) async fn encode_message1(&mut self, payload: &[u8]) -> Result<Vec<u8>> {
        let mut state = self.state.clone();
        // output e.pubKey
        let e_pub_key = self.get_public_key(state.e()?).await?;
        state.mix_hash(e_pub_key.data());
        let mut message = e_pub_key.data().to_vec();

        // output message 1 payload
        message.extend_from_slice(payload);
        state.mix_hash(payload);

        self.state = state;
        Ok(message)
    }

    /// Decode the first message to get the ephemeral public key sent by the initiator
    pub(super) async fn decode_message1(&mut self, message: &[u8]) -> Result<Vec<u8>> {
        let mut state = self.state.clone();
        // read e.pubKey
        let key = Self::read_key(message)?;
        state.mix_hash(key);

        state.re = Some(PublicKey::new(key.to_vec(), X25519));

        // decode payload
        let payload = Self::read_message1_payload(message)?;
        state.mix_hash(payload);

        self.state = state;
        Ok(payload.to_vec())
    }

    /// Encode the second message from the responder to the initiator
    /// That message contains: the responder ephemeral public key + a Diffie-Hellman key +
    ///   an encrypted payload containing the responder identity / signature / credentials
    pub(super) async fn encode_message2(&mut self, payload: &[u8]) -> Result<Vec<u8>> {
        let mut state = self.state.clone();
        // output e.pubKey
        let e_pub_key = self.get_public_key(state.e()?).await?;
        state.mix_hash(e_pub_key.data());
        let mut message2 = e_pub_key.data().to_vec();

        // ck, k = HKDF(ck, DH(e, re), 2)
        let dh = self.dh(state.e()?, state.re()?).await?;
        self.hkdf(&mut state, dh).await?;

        // encrypt and output s.pubKey
        let s_pub_key = self.get_public_key(state.s()?).await?;
        let c = self.encrypt_and_hash(&mut state, s_pub_key.data()).await?;
        message2.extend_from_slice(c.as_slice());

        // ck, k = HKDF(ck, DH(s, re), 2)
        let dh = self.dh(state.s()?, state.re()?).await?;
        self.hkdf(&mut state, dh).await?;

        // encrypt and output payload
        let c = self.encrypt_and_hash(&mut state, payload).await?;
        message2.extend(c);
        self.state = state;
        Ok(message2)
    }

    /// Decode the second message sent by the responder
    pub(super) async fn decode_message2(&mut self, message: &[u8]) -> Result<Vec<u8>> {
        let mut state = self.state.clone();
        // decode re.pubKey
        let re_pub_key = Self::read_key(message)?;
        state.re = Some(PublicKey::new(re_pub_key.to_vec(), X25519));
        state.mix_hash(re_pub_key);

        // ck, k = HKDF(ck, DH(e, re), 2)
        let dh = self.dh(state.e()?, state.re()?).await?;
        self.hkdf(&mut state, dh).await?;

        // decrypt rs.pubKey
        let rs_pub_key = Self::read_message2_encrypted_key(message)?;
        state.rs = Some(PublicKey::new(
            self.hash_and_decrypt(&mut state, rs_pub_key).await?,
            X25519,
        ));

        // ck, k = HKDF(ck, DH(e, rs), 2)
        let dh = self.dh(state.e()?, state.rs()?).await?;
        self.hkdf(&mut state, dh).await?;

        // decrypt payload
        let c = Self::read_message2_payload(message)?;
        let payload = self.hash_and_decrypt(&mut state, c).await?;

        self.state = state;
        Ok(payload)
    }

    /// Encode the third message from the initiator to the responder
    /// That message contains: the initiator static public key (encrypted) + a Diffie-Hellman key +
    ///   an encrypted payload containing the initiator identity / signature / credentials
    pub(super) async fn encode_message3(&mut self, payload: &[u8]) -> Result<Vec<u8>> {
        let mut state = self.state.clone();
        // encrypt s.pubKey
        let s_pub_key = self.get_public_key(state.s()?).await?;
        let c = self.encrypt_and_hash(&mut state, s_pub_key.data()).await?;
        let mut message3 = c.to_vec();

        // ck, k = HKDF(ck, DH(s, re), 2)
        let dh = self.dh(state.s()?, state.re()?).await?;
        self.hkdf(&mut state, dh).await?;

        // encrypt payload
        let c = self.encrypt_and_hash(&mut state, payload).await?;
        message3.extend(c);

        self.state = state;
        Ok(message3)
    }

    /// Decode the third message sent by the initiator
    pub(super) async fn decode_message3(&mut self, message: &[u8]) -> Result<Vec<u8>> {
        let mut state = self.state.clone();
        // decrypt rs key
        let rs_pub_key = Self::read_message3_encrypted_key(message)?;
        state.rs = Some(PublicKey::new(
            self.hash_and_decrypt(&mut state, rs_pub_key).await?,
            X25519,
        ));

        // ck, k = HKDF(ck, DH(e, rs), 2), n = 0
        let dh = self.dh(state.e()?, state.rs()?).await?;
        self.hkdf(&mut state, dh).await?;

        // decrypt payload
        let c = Self::read_message3_payload(message)?;
        let payload = self.hash_and_decrypt(&mut state, c).await?;
        self.state = state;
        Ok(payload)
    }

    /// Set the final state of the state machine by creating the encryption / decryption keys
    /// and return the other party identity
    pub(super) async fn set_final_state(&mut self, role: Role) -> Result<()> {
        // k1, k2 = HKDF(ck, zerolen, 2)
        let mut state = self.state.clone();
        let (k1, k2) = self.compute_final_keys(&mut state).await?;
        let (encryption_key, decryption_key) = if role.is_initiator() {
            (k2, k1)
        } else {
            (k1, k2)
        };
        state.status = Ready(HandshakeKeys {
            encryption_key,
            decryption_key,
        });
        // now remove the ephemeral keys which are not useful anymore
        self.state = state;
        self.delete_ephemeral_keys().await?;
        Ok(())
    }

    /// Return the final results of the handshake if we reached the final state
    pub(super) fn get_handshake_keys(&self) -> Option<HandshakeKeys> {
        match &self.state.status {
            Ready(keys) => Some(keys.clone()),
            _ => None,
        }
    }
}

impl Handshake {
    /// Create a new handshake
    pub(super) async fn new(
        vault: Arc<dyn SecureChannelVault>,
        static_key: KeyId,
    ) -> Result<Handshake> {
        // 1. generate an ephemeral key pair for this handshake and set it to e
        let ephemeral_key = Self::generate_ephemeral_key(vault.clone()).await?;

        // 2. initialize the handshake
        // We currently don't use any payload for message 1
        Ok(Handshake {
            vault,
            state: HandshakeState::new(static_key, ephemeral_key),
        })
    }

    /// Import the k secret
    async fn import_k_secret(&self, content: Vec<u8>) -> Result<KeyId> {
        self.vault
            .import_ephemeral_secret(Secret::new(content), Self::k_attributes())
            .await
    }

    /// Import the ck secret
    async fn import_ck_secret(&self, content: Vec<u8>) -> Result<KeyId> {
        self.vault
            .import_ephemeral_secret(Secret::new(content), Self::ck_attributes())
            .await
    }

    /// Return the public key corresponding to a given key id
    async fn get_public_key(&self, key_id: &KeyId) -> Result<PublicKey> {
        self.vault.get_public_key(key_id).await
    }

    /// Compute a Diffie-Hellman key between a given key id and the other party public key
    async fn dh(&self, key_id: &KeyId, public_key: &PublicKey) -> Result<KeyId> {
        self.vault.ec_diffie_hellman(key_id, public_key).await
    }

    /// Compute two derived ck, and k keys based on existing ck and k keys + a Diffie-Hellman key
    async fn hkdf(&self, state: &mut HandshakeState, dh: KeyId) -> Result<()> {
        let hkdf_output = self
            .vault
            .hkdf_sha256(
                state.ck()?,
                b"",
                Some(&dh),
                vec![Self::ck_attributes(), Self::k_attributes()],
            )
            .await?;

        // The Diffie-Hellman secret is not useful anymore
        // we can delete it from memory
        self.vault.delete_secret(dh).await?;

        let [new_ck, new_k]: [KeyId; 2] = hkdf_output
            .try_into()
            .map_err(|_| XXError::InternalVaultError)?;

        let old_ck = state.take_ck()?;
        state.ck = Some(new_ck);
        self.vault.delete_secret(old_ck).await?;

        let old_k = state.take_k()?;
        state.k = Some(new_k);
        self.vault.delete_secret(old_k).await?;

        state.n = 0;
        Ok(())

        //_ => ,
    }

    /// Compute the final encryption and decryption keys
    async fn compute_final_keys(&self, state: &mut HandshakeState) -> Result<(KeyId, KeyId)> {
        let hkdf_output = self
            .vault
            .hkdf_sha256(
                state.ck()?,
                b"",
                None,
                vec![Self::k_attributes(), Self::k_attributes()],
            )
            .await?;

        let [k1, k2]: [KeyId; 2] = hkdf_output
            .try_into()
            .map_err(|_| XXError::InternalVaultError)?;

        self.vault.delete_secret(state.take_ck()?).await?;
        self.vault.delete_secret(state.take_k()?).await?;

        Ok((k1, k2))
    }

    /// Decrypt a ciphertext 'c' using the key 'k' and the additional data 'h'
    async fn hash_and_decrypt(&self, state: &mut HandshakeState, c: &[u8]) -> Result<Vec<u8>> {
        let mut nonce = [0u8; 12];
        nonce[4..].copy_from_slice(&state.n.to_be_bytes());
        let result = self
            .vault
            .aead_aes_gcm_decrypt(state.k()?, c, nonce.as_ref(), &state.h)
            .await
            .map(|b| b.to_vec())?;
        state.mix_hash(c);
        state.n += 1;
        Ok(result)
    }

    /// Encrypt a plaintext 'c' using the key 'k' and the additional data 'h'
    async fn encrypt_and_hash(&self, state: &mut HandshakeState, p: &[u8]) -> Result<Vec<u8>> {
        let mut nonce = [0u8; 12];
        nonce[4..].copy_from_slice(&state.n.to_be_bytes());

        let result = self
            .vault
            .aead_aes_gcm_encrypt(state.k()?, p, nonce.as_ref(), &state.h)
            .await?
            .to_vec();
        state.mix_hash(result.as_slice());
        state.n += 1;
        Ok(result)
    }

    async fn delete_ephemeral_keys(&mut self) -> Result<()> {
        _ = self.vault.delete_secret(self.state.take_e()?).await?;

        Ok(())
    }
}

/// Static functions
impl Handshake {
    /// Protocol name, used as a secret during the handshake initialization, padded to 32 bytes
    fn protocol_name() -> &'static [u8; 32] {
        b"Noise_XX_25519_AESGCM_SHA256\0\0\0\0"
    }

    /// Generate an ephemeral key for the key exchange
    async fn generate_ephemeral_key(vault: Arc<dyn SecureChannelVault>) -> Result<KeyId> {
        vault
            .generate_ephemeral_secret(SecretAttributes::X25519)
            .await
    }

    /// Secret attributes for the ck key
    fn ck_attributes() -> SecretAttributes {
        SecretAttributes::Buffer(SHA256_SIZE_U32)
    }

    /// Secret attributes for the k key
    fn k_attributes() -> SecretAttributes {
        SecretAttributes::Aes256
    }

    /// Read the message 1 payload which is present after the public key
    fn read_message1_payload(message: &[u8]) -> Result<&[u8]> {
        Self::read_end(message, Self::key_size())
    }

    /// Read the message 2 encrypted key, which is present after the public key
    fn read_message2_encrypted_key(message: &[u8]) -> Result<&[u8]> {
        Self::read_middle(message, Self::key_size(), Self::encrypted_key_size())
    }

    /// Read the message 2 encrypted payload, which is present after the encrypted key
    fn read_message2_payload(message: &[u8]) -> Result<&[u8]> {
        Self::read_end(message, Self::key_size() + Self::encrypted_key_size())
    }

    /// Read the message 3 encrypted key at the beginning of the message
    fn read_message3_encrypted_key(message: &[u8]) -> Result<&[u8]> {
        Self::read_start(message, Self::encrypted_key_size())
    }

    /// Read the message 3 payload which is present after the encrypted key
    fn read_message3_payload(message: &[u8]) -> Result<&[u8]> {
        Self::read_end(message, Self::encrypted_key_size())
    }

    /// Read the first 'length' bytes of the message
    fn read_start(message: &[u8], length: usize) -> Result<&[u8]> {
        if message.len() < length {
            return Err(XXError::MessageLenMismatch.into());
        }
        Ok(&message[0..length])
    }

    /// Read the bytes of the message after the first 'drop_length' bytes
    fn read_end(message: &[u8], drop_length: usize) -> Result<&[u8]> {
        if message.len() < drop_length {
            return Err(XXError::MessageLenMismatch.into());
        }
        Ok(&message[drop_length..])
    }

    /// Read 'length' bytes of the message after the first 'drop_length' bytes
    fn read_middle(message: &[u8], drop_length: usize, length: usize) -> Result<&[u8]> {
        if message.len() < drop_length + length {
            return Err(XXError::MessageLenMismatch.into());
        }
        Ok(&message[drop_length..(drop_length + length)])
    }

    /// Read the bytes of a key at the beginning of a message
    fn read_key(message: &[u8]) -> Result<&[u8]> {
        Self::read_start(message, Self::key_size())
    }

    /// Size of a public key
    fn key_size() -> usize {
        X25519_PUBLIC_LENGTH_USIZE
    }

    /// Size of an encrypted key
    fn encrypted_key_size() -> usize {
        Self::key_size() + AES_GCM_TAGSIZE_USIZE
    }
}

/// The `HandshakeState` contains all the variables necessary to follow the Noise protocol
#[derive(Debug, Clone)]
pub(super) struct HandshakeState {
    pub(super) s: Option<KeyId>,
    e: Option<KeyId>,
    k: Option<KeyId>,
    re: Option<PublicKey>,
    pub(super) rs: Option<PublicKey>,
    n: u64,
    h: [u8; SHA256_SIZE_USIZE],
    ck: Option<KeyId>,
    pub(super) status: Status,
}

impl HandshakeState {
    /// Create a new HandshakeState with:
    ///   - a static key
    ///   - an ephemeral key
    ///   - a payload
    pub(super) fn new(s: KeyId, e: KeyId) -> HandshakeState {
        HandshakeState {
            s: Some(s),
            e: Some(e),
            k: None,
            re: None,
            rs: None,
            n: 0,
            h: [0u8; SHA256_SIZE_USIZE],
            ck: None,
            status: Initial,
        }
    }

    /// h = SHA256(h || data)
    pub(super) fn mix_hash(&mut self, data: &[u8]) {
        let mut input = Vec::with_capacity(SHA256_SIZE_USIZE + data.len());
        input.extend_from_slice(&self.h);
        input.extend_from_slice(data);
        self.h = Self::sha256(&input);
    }

    pub(super) fn sha256(data: &[u8]) -> [u8; 32] {
        let digest = Sha256::digest(data);
        *array_ref![digest, 0, 32]
    }

    pub(super) fn take_e(&mut self) -> Result<KeyId> {
        self.e.take().ok_or_else(|| {
            Error::new(
                Origin::KeyExchange,
                Kind::Invalid,
                "key id e should have been set",
            )
        })
    }

    pub(super) fn take_k(&mut self) -> Result<KeyId> {
        self.k.take().ok_or_else(|| {
            Error::new(
                Origin::KeyExchange,
                Kind::Invalid,
                "key id k should have been set",
            )
        })
    }

    pub(super) fn take_ck(&mut self) -> Result<KeyId> {
        self.ck.take().ok_or_else(|| {
            Error::new(
                Origin::KeyExchange,
                Kind::Invalid,
                "key id ck should have been set",
            )
        })
    }

    pub(super) fn s(&self) -> Result<&KeyId> {
        self.s.as_ref().ok_or_else(|| {
            Error::new(
                Origin::KeyExchange,
                Kind::Invalid,
                "key id s should have been set",
            )
        })
    }

    pub(super) fn e(&self) -> Result<&KeyId> {
        self.e.as_ref().ok_or_else(|| {
            Error::new(
                Origin::KeyExchange,
                Kind::Invalid,
                "key id e should have been set",
            )
        })
    }

    pub(super) fn k(&self) -> Result<&KeyId> {
        self.k.as_ref().ok_or_else(|| {
            Error::new(
                Origin::KeyExchange,
                Kind::Invalid,
                "key id k should have been set",
            )
        })
    }

    pub(super) fn ck(&self) -> Result<&KeyId> {
        self.ck.as_ref().ok_or_else(|| {
            Error::new(
                Origin::KeyExchange,
                Kind::Invalid,
                "key id ck should have been set",
            )
        })
    }

    pub(super) fn re(&self) -> Result<&PublicKey> {
        self.re.as_ref().ok_or_else(|| {
            Error::new(
                Origin::KeyExchange,
                Kind::Invalid,
                "public key id re should have been set",
            )
        })
    }

    pub(super) fn rs(&self) -> Result<&PublicKey> {
        self.rs.as_ref().ok_or_else(|| {
            Error::new(
                Origin::KeyExchange,
                Kind::Invalid,
                "public key id rs should have been set",
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identities;
    use hex::decode;
    use ockam_core::Result;
    use ockam_node::InMemoryKeyValueStorage;
    use ockam_vault::SoftwareSecureChannelVault;

    #[tokio::test]
    async fn test_initialization() -> Result<()> {
        let vault = Arc::new(SoftwareSecureChannelVault::new(
            InMemoryKeyValueStorage::create(),
        ));

        let static_key = vault
            .generate_static_secret(SecretAttributes::X25519)
            .await?;
        let mut handshake = Handshake::new(vault.clone(), static_key).await?;
        handshake.initialize().await?;

        let exp_h = [
            93, 247, 43, 103, 185, 101, 173, 209, 22, 143, 10, 108, 117, 109, 242, 28, 32, 79, 126,
            100, 252, 104, 43, 230, 163, 171, 75, 104, 44, 141, 182, 75,
        ];

        assert_eq!(handshake.state.h, exp_h);

        let ck = vault.get_ephemeral_secret(handshake.state.ck()?)?;

        assert_eq!(
            ck.secret().as_ref(),
            *b"Noise_XX_25519_AESGCM_SHA256\0\0\0\0"
        );
        assert_eq!(handshake.state.n, 0);
        Ok(())
    }

    #[tokio::test]
    async fn test_full_handshake1() -> Result<()> {
        let handshake_messages = HandshakeMessages {
            initiator_static_key: decode("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f").unwrap(),
            initiator_ephemeral_key: decode("202122232425262728292a2b2c2d2e2f303132333435363738393a3b3c3d3e3f").unwrap(),
            responder_static_key: decode("0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f20").unwrap(),
            responder_ephemeral_key: decode("4142434445464748494a4b4c4d4e4f505152535455565758595a5b5c5d5e5f60").unwrap(),
            message1_payload: decode("").unwrap(),
            message1_ciphertext: decode("358072d6365880d1aeea329adf9121383851ed21a28e3b75e965d0d2cd166254").unwrap(),
            message2_payload: decode("").unwrap(),
            message2_ciphertext: decode("64b101b1d0be5a8704bd078f9895001fc03e8e9f9522f188dd128d9846d484665393019dbd6f438795da206db0886610b26108e424142c2e9b5fd1f7ea70cde8767ce62d7e3c0e9bcefe4ab872c0505b9e824df091b74ffe10a2b32809cab21f").unwrap(),
            message3_payload: decode("").unwrap(),
            message3_ciphertext: decode("e610eadc4b00c17708bf223f29a66f02342fbedf6c0044736544b9271821ae40e70144cecd9d265dffdc5bb8e051c3f83db32a425e04d8f510c58a43325fbc56").unwrap(),
        };

        check_handshake(handshake_messages).await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_full_handshake2() -> Result<()> {
        let handshake_messages = HandshakeMessages {
            initiator_static_key: decode("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f").unwrap(),
            initiator_ephemeral_key: decode("202122232425262728292a2b2c2d2e2f303132333435363738393a3b3c3d3e3f").unwrap(),
            responder_static_key: decode("0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f20").unwrap(),
            responder_ephemeral_key: decode("4142434445464748494a4b4c4d4e4f505152535455565758595a5b5c5d5e5f60").unwrap(),
            message1_payload: decode("746573745f6d73675f30").unwrap(),
            message1_ciphertext: decode("358072d6365880d1aeea329adf9121383851ed21a28e3b75e965d0d2cd166254746573745f6d73675f30").unwrap(),
            message2_payload: decode("746573745f6d73675f31").unwrap(),
            message2_ciphertext: decode("64b101b1d0be5a8704bd078f9895001fc03e8e9f9522f188dd128d9846d484665393019dbd6f438795da206db0886610b26108e424142c2e9b5fd1f7ea70cde8c9f29dcec8d3ab554f4a5330657867fe4917917195c8cf360e08d6dc5f71baf875ec6e3bfc7afda4c9c2").unwrap(),
            message3_payload: decode("746573745f6d73675f32").unwrap(),
            message3_ciphertext: decode("e610eadc4b00c17708bf223f29a66f02342fbedf6c0044736544b9271821ae40232c55cd96d1350af861f6a04978f7d5e070c07602c6b84d25a331242a71c50ae31dd4c164267fd48bd2").unwrap(),
        };

        check_handshake(handshake_messages).await?;
        Ok(())
    }

    // --------------------
    // TESTS IMPLEMENTATION
    // --------------------

    struct HandshakeMessages {
        initiator_static_key: Vec<u8>,
        initiator_ephemeral_key: Vec<u8>,
        responder_static_key: Vec<u8>,
        responder_ephemeral_key: Vec<u8>,
        message1_payload: Vec<u8>,
        message1_ciphertext: Vec<u8>,
        message2_payload: Vec<u8>,
        message2_ciphertext: Vec<u8>,
        message3_payload: Vec<u8>,
        message3_ciphertext: Vec<u8>,
    }

    async fn check_handshake(messages: HandshakeMessages) -> Result<()> {
        let vault = identities().vault();

        let initiator_static_key_id = vault
            .secure_channel_vault
            .import_static_secret(
                Secret::new(messages.initiator_static_key),
                SecretAttributes::X25519,
            )
            .await?;
        let initiator_ephemeral_key_id = vault
            .secure_channel_vault
            .import_ephemeral_secret(
                Secret::new(messages.initiator_ephemeral_key),
                SecretAttributes::X25519,
            )
            .await?;
        let mut initiator = Handshake::new_with_keys(
            vault.secure_channel_vault.clone(),
            initiator_static_key_id,
            initiator_ephemeral_key_id,
        )
        .await?;

        let responder_static_key_id = vault
            .secure_channel_vault
            .import_static_secret(
                Secret::new(messages.responder_static_key),
                SecretAttributes::X25519,
            )
            .await?;
        let responder_ephemeral_key_id = vault
            .secure_channel_vault
            .import_ephemeral_secret(
                Secret::new(messages.responder_ephemeral_key),
                SecretAttributes::X25519,
            )
            .await?;
        let mut responder = Handshake::new_with_keys(
            vault.secure_channel_vault.clone(),
            responder_static_key_id,
            responder_ephemeral_key_id,
        )
        .await?;
        initiator.initialize().await?;
        responder.initialize().await?;

        let result = initiator
            .encode_message1(&messages.message1_payload)
            .await?;
        assert_eq!(result, messages.message1_ciphertext);

        let decoded = responder.decode_message1(&result).await?;
        assert_eq!(decoded, messages.message1_payload);

        let result = responder
            .encode_message2(&messages.message2_payload)
            .await?;
        assert_eq!(result, messages.message2_ciphertext);

        let decoded = initiator.decode_message2(&result).await?;
        assert_eq!(decoded, messages.message2_payload);

        let result = initiator
            .encode_message3(&messages.message3_payload)
            .await?;
        assert_eq!(result, messages.message3_ciphertext);

        let decoded = responder.decode_message3(&result).await?;
        assert_eq!(decoded, messages.message3_payload);

        let result = initiator.set_final_state(Role::Responder).await;
        assert!(result.is_ok());

        let result = responder.set_final_state(Role::Initiator).await;
        assert!(result.is_ok());

        Ok(())
    }

    impl Handshake {
        /// Initialize the handshake
        async fn new_with_keys(
            vault: Arc<dyn SecureChannelVault>,
            static_key: KeyId,
            ephemeral_key: KeyId,
        ) -> Result<Handshake> {
            Ok(Handshake {
                vault,
                state: HandshakeState::new(static_key, ephemeral_key),
            })
        }
    }
}
