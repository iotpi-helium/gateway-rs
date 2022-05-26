use crate::{
    error::OnionError, traits::Base64, Error, Keypair, MsgSign, Packet, PublicKey, Result,
};
use aes_gcm::{
    aes::{
        cipher::{FromBlockCipher, StreamCipher},
        Aes256, BlockEncrypt, NewBlockCipher,
    },
    Tag, C_MAX,
};
use bytes::{Buf, BufMut, Bytes, BytesMut};
use ctr::Ctr32BE;
use ghash::{
    universal_hash::{NewUniversalHash, UniversalHash},
    GHash,
};
use helium_proto::{
    gateway_poc_report_req_v1, BlockchainPocWitnessV1, BlockchainRegionParamsV1,
    GatewayPocReportReqV1,
};
use semtech_udp::DataRate;
use sha2::{Digest, Sha256, Sha512};
use std::{str::FromStr, sync::Arc};

pub const TAG_LENGTH: usize = 4;
pub const NONCE_LENGTH: usize = 12;

#[derive(Debug)]
pub struct Onion {
    pub signal_strength: f32,
    pub snr: f32,
    pub timestamp: u64,
    pub frequency: f32,
    pub channel: i32,
    pub datarate: DataRate,
    pub public_key: PublicKey,
    pub iv: u16,
    pub tag: [u8; TAG_LENGTH],
    pub cipher_text: Vec<u8>,
}

impl Onion {
    pub fn from_bytes(
        buf: &[u8],
        region_params: Option<&BlockchainRegionParamsV1>,
    ) -> Result<Self> {
        if region_params.is_none() {
            return Err(OnionError::no_region_params());
        }
        let mut data = Bytes::copy_from_slice(buf);
        if data.len() < 39 {
            return Err(Error::custom("invalid onion size"));
        }
        let iv = data.get_u16_le();
        let public_key_bin = data.copy_to_bytes(33);
        let public_key =
            PublicKey::from_bytes(&public_key_bin).map_err(|_| OnionError::invalid_key())?;
        let mut tag = [0u8; 4];
        data.copy_to_slice(&mut tag);
        let cipher_text = data.to_vec();
        Ok(Self {
            signal_strength: 0.0,
            snr: 0.0,
            frequency: 0.0,
            channel: 0,
            timestamp: 0,
            datarate: DataRate::default(),
            public_key,
            iv,
            tag,
            cipher_text,
        })
    }

    pub fn from_packet(
        packet: &Packet,
        region_params: Option<&BlockchainRegionParamsV1>,
    ) -> Result<Self> {
        let mut result = Self::from_bytes(&packet.payload, region_params)?;
        result.signal_strength = packet.signal_strength;
        result.snr = packet.snr;
        result.timestamp = packet.timestamp;
        result.frequency = packet.frequency;
        result.datarate = DataRate::from_str(&packet.datarate)?;
        result.channel = Self::channel(packet.frequency, region_params.unwrap())
            .ok_or_else(|| Error::custom("no valid channel in region params"))?;
        Ok(result)
    }

    fn channel(frequency: f32, region_params: &BlockchainRegionParamsV1) -> Option<i32> {
        let mut channel: i32 = 0;
        for region_param in &region_params.region_params {
            if (region_param.channel_frequency as f64 - frequency as f64).abs() <= 0.001 {
                return Some(channel);
            } else {
                channel += 1;
            }
        }
        None
    }

    pub async fn as_witness_report(&self, keypair: Arc<Keypair>) -> Result<GatewayPocReportReqV1> {
        let mut packet_hash = Sha256::new();
        packet_hash.update(&self.tag);
        packet_hash.update(&self.cipher_text);

        let mut witness = BlockchainPocWitnessV1 {
            packet_hash: packet_hash.finalize().to_vec(),
            gateway: keypair.public_key().to_vec(),
            signal: self.signal_strength as i32,
            datarate: self.datarate.to_string(),
            frequency: self.frequency,
            snr: self.snr,
            channel: self.channel,
            timestamp: self.timestamp,
            signature: vec![],
        };
        witness.signature = witness.sign(keypair).await?;
        Ok(GatewayPocReportReqV1 {
            onion_key_hash: self.poc_id().into(),
            msg: Some(gateway_poc_report_req_v1::Msg::Witness(witness)),
        })
    }

    pub fn poc_id(&self) -> PocId {
        PocId::from(self)
    }

    pub fn get_layer(&self) -> Result<(u16, Vec<u8>)> {
        let mut buf = Bytes::copy_from_slice(&self.cipher_text);
        let data_size = buf.get_u8() as usize;
        let mut data = buf.copy_to_bytes(data_size);
        let rest = buf;

        let mut digest = Bytes::from(Sha512::digest(&data).to_vec());
        let padding = digest.copy_to_bytes(data_size + 5);
        let xor = digest.get_u16_le();

        let mut next_layer = BytesMut::with_capacity(50);
        next_layer.put_u16_le(self.iv ^ xor);
        next_layer.put(&self.public_key.to_vec()[..]);
        next_layer.put(rest);
        next_layer.put(padding);

        Ok((data.get_u16_le(), next_layer.to_vec()))
    }
}

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct PocId(Vec<u8>);

impl From<&Onion> for PocId {
    fn from(v: &Onion) -> Self {
        Self(Sha256::digest(&v.public_key.to_vec()).to_vec())
    }
}

impl From<Vec<u8>> for PocId {
    fn from(v: Vec<u8>) -> Self {
        Self(v)
    }
}

impl From<PocId> for Vec<u8> {
    fn from(v: PocId) -> Self {
        v.0
    }
}

impl AsRef<[u8]> for PocId {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl ToString for PocId {
    fn to_string(&self) -> String {
        self.0.to_b64()
    }
}

impl slog::Value for PocId {
    fn serialize(
        &self,
        _record: &slog::Record,
        key: slog::Key,
        serializer: &mut dyn slog::Serializer,
    ) -> slog::Result {
        serializer.emit_str(key, &self.to_string())
    }
}

impl Onion {
    pub fn decrypt_in_place(&mut self, keypair: Arc<Keypair>) -> Result {
        if self.cipher_text.len() as u64 > C_MAX {
            return Err(OnionError::invalid_size(self.cipher_text.len()));
        }

        let onion_keybin = self.public_key.to_vec();

        let mut aad = [0u8; NONCE_LENGTH + helium_crypto::ecc_compact::PUBLIC_KEY_LENGTH];

        {
            let mut aad = &mut aad[10..];
            aad.put_u16_le(self.iv);
            aad.put_slice(&onion_keybin);
        }

        let shared_secret = keypair.ecdh(&self.public_key)?;
        let cipher = Aes256::new(shared_secret.as_bytes());
        let nonce = &aad[..NONCE_LENGTH];

        let mut expected_tag = self.compute_tag(&cipher, &aad, &self.cipher_text);
        let mut ctr = self.init_ctr(&cipher, nonce);
        ctr.apply_keystream(expected_tag.as_mut_slice());

        if !expected_tag.starts_with(&self.tag) {
            return Err(OnionError::crypto_error());
        }

        ctr.apply_keystream(&mut self.cipher_text);
        Ok(())
    }

    /// Initialize counter mode.
    ///
    /// Taken from
    /// [aes-gcm](https://github.com/RustCrypto/AEADs/blob/master/aes-gcm/src/lib.rs)
    /// since that crate doesn't allow for shorter tag comparisons on decrypt
    fn init_ctr<'a>(&self, cipher: &'a Aes256, nonce: &[u8]) -> Ctr32BE<&'a Aes256> {
        let j0 = {
            let mut block = ghash::Block::default();
            block[..12].copy_from_slice(nonce);
            block[15] = 1;
            block
        };
        Ctr32BE::from_block_cipher(cipher, &j0)
    }

    /// Compute tag
    ///
    /// Taken from
    /// [aes-gcm](https://github.com/RustCrypto/AEADs/blob/master/aes-gcm/src/lib.rs)
    /// since it's not exposed
    fn compute_tag(&self, cipher: &Aes256, aad: &[u8], buffer: &[u8]) -> Tag {
        let mut ghash_key = ghash::Key::default();
        cipher.encrypt_block(&mut ghash_key);

        let mut ghash = GHash::new(&ghash_key);

        ghash.update_padded(aad);
        ghash.update_padded(buffer);

        let associated_data_bits = (aad.len() as u64) * 8;
        let buffer_bits = (buffer.len() as u64) * 8;

        let mut block = ghash::Block::default();
        block[..8].copy_from_slice(&associated_data_bits.to_be_bytes());
        block[8..].copy_from_slice(&buffer_bits.to_be_bytes());
        ghash.update(&block);
        ghash.finalize().into_bytes()
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use hex_literal::hex;

    #[test]
    fn decrypt() {
        // Consructed using libp2p_crypto as the gateway keypaor
        const GW_KEYPAIR: &[u8] = &hex!("004956DB80645842ED4C938BD625DF3D99674729C6D9021025C8390C5ACFB93A4404911E9B3E4199F61BF47736D01100D5DF0FF57BCCBF61BDA4DF6CCA51B62040F409D50F6890CB91B513CAE429054C5E068DF44DC80DCE43EF361DD2E6530BBA81");
        // Constructed by creating an onion keypair in libp2p_crypto and copying
        // the pubkey_to_bin
        const ONION_PUBKEY: &[u8] =
            &hex!("00A8731EAD55027001185D153258530E682EA66374357C28D181E542AA497E4415");
        // Constructed by doing an ECDH with the onion private key and the
        // public gw key from the keypair to get the shared secret.
        // Then using the shared secret to call
        //
        // Plaintext = "hello world".
        // IV0 = 42.
        // IV = <<0:80/integer, IV0:16/integer-unsigned-little>>.
        // OnionPubKeyBin = libp2p_crypto:pubkey_to_bin(OnionPubKey).
        // AAD = <<IV/binary, OnionPubKeyBin/binary>>.
        // {CipherText, Tag} = crypto:crypto_one_time_aead(aes_256_gcm, SharedSecret, IV, PlainText, AAD, 4, true).
        //
        // To encrypt the content to thsi cipher text and tag
        const CIPHER_TEXT: &[u8] = &hex!("F3E49EB69F2783A1A087C9");
        const TAG: [u8; 4] = hex!("3E031987");

        let gw_keypair = helium_crypto::Keypair::try_from(GW_KEYPAIR).expect("gw keypair");
        let onion_pubkey = helium_crypto::PublicKey::try_from(ONION_PUBKEY).expect("onion pubkey");

        let mut onion = Onion {
            signal_strength: 0_f32,
            snr: 0_f32,
            datarate: DataRate::from_str("SF7BW500").expect("datarate"),
            timestamp: 0,
            channel: 0,
            frequency: 0_f32,
            iv: 42,
            public_key: onion_pubkey,
            tag: TAG,
            cipher_text: CIPHER_TEXT.to_vec(),
        };
        onion
            .decrypt_in_place(Arc::new(gw_keypair.into()))
            .expect("decrypt");

        assert_eq!(
            "hello world".to_string(),
            String::from_utf8(onion.cipher_text).expect("plain text")
        );
    }
}
