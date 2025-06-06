//
// Copyright 2019-2021 Signal Messenger, LLC
// SPDX-License-Identifier: AGPL-3.0-only
//

use std::{collections::HashMap, mem::size_of};

use aes::Aes256;
use ctr::cipher::{KeyIvInit, StreamCipher};
use hkdf::Hkdf;
use hmac::{Hmac, Mac as _};
use rand::{CryptoRng, Rng};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use thiserror::Error;

#[derive(Error, Debug, Eq, PartialEq)]
pub enum Error {
    #[error("no receiver state could be found matching the provided data")]
    NoMatchingReceiverState,
}

const RATCHET_INFO_STRING: &[u8; 15] = b"RingRTC Ratchet";
const MAX_RECEIVER_STATES_TO_RETAIN: usize = 5;
/// Maximum number of out of order frames to keep old ratchet keys for.
/// Accommodate up to 30 frames per second for 10 seconds worth of keys.
const MAX_OOO_FRAMES: u64 = 30 * 10;
/// Maximum number of out of order ratchets to keep old ratchet keys for.
const MAX_OOO_RATCHETS: u8 = 5;
pub const MAC_SIZE_BYTES: usize = 16;

// For some reason the linter doesn't detect this is required in the static assertions.
#[allow(dead_code)]
const HMAC_SHA256_SIZE_BYTES: usize = 256 / 8;

type HmacSha256 = Hmac<Sha256>;
type Aes256Ctr = ctr::Ctr64BE<Aes256>;
type AesKey = [u8; 32];
type HmacKey = [u8; 32];
type Iv = [u8; 16];
pub type Secret = [u8; 32];
pub type RatchetCounter = u8;
pub type SenderId = u32;
pub type FrameCounter = u64;
pub type Mac = [u8; MAC_SIZE_BYTES];

pub fn random_secret<R: Rng + CryptoRng + ?Sized>(rng: &mut R) -> Secret {
    let mut secret = Secret::default();
    rng.fill(&mut secret[..]);
    secret
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
struct SenderState {
    current_aes_key: AesKey,
    current_hmac_key: HmacKey,
    current_secret: Secret,
    ratchet_counter: RatchetCounter,
}

impl SenderState {
    fn new(ratchet_counter: RatchetCounter, secret: Secret) -> Self {
        let mut result = Self {
            current_aes_key: [0u8; size_of::<AesKey>()],
            current_hmac_key: [0u8; size_of::<HmacKey>()],
            current_secret: secret,
            ratchet_counter,
        };
        result.derive_aes_key();
        result.derive_hmac_key();
        result
    }

    fn mut_advance_ratchet(&mut self) {
        let secret_hkdf = Hkdf::<Sha256>::new(None, &self.current_secret[..]);
        secret_hkdf
            .expand(RATCHET_INFO_STRING, &mut self.current_secret[..])
            .unwrap_or_else(|_| {
                panic!(
                    "HKDF should work with output of length {}",
                    std::mem::size_of::<Secret>()
                )
            });
        self.derive_aes_key();
        self.derive_hmac_key();
        self.ratchet_counter = self.ratchet_counter.wrapping_add(1);
    }

    fn derive_aes_key(&mut self) {
        let key_hkdf = Hkdf::<Sha256>::new(None, &self.current_secret[..]);
        key_hkdf
            .expand(b"RingRTC AES Key", &mut self.current_aes_key[..])
            .unwrap_or_else(|_| {
                panic!(
                    "HKDF should work with output of length {}",
                    std::mem::size_of::<AesKey>()
                )
            });
    }

    fn derive_hmac_key(&mut self) {
        let hmac_hkdf = Hkdf::<Sha256>::new(None, &self.current_secret[..]);
        hmac_hkdf
            .expand(b"RingRTC HMAC Key", &mut self.current_hmac_key[..])
            .unwrap_or_else(|_| {
                panic!(
                    "HKDF should work with output of length {}",
                    std::mem::size_of::<HmacKey>()
                )
            });
    }
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
struct ReceiverState {
    sender_state: SenderState,
    ratchet_frame: FrameCounter,
    old_secret: Secret,
    old_ratchet_counter: RatchetCounter,
}

impl ReceiverState {
    fn new(ratchet_counter: RatchetCounter, secret: Secret) -> Self {
        Self {
            sender_state: SenderState::new(ratchet_counter, secret),
            ratchet_frame: 0,
            old_secret: secret,
            old_ratchet_counter: ratchet_counter,
        }
    }

    fn try_advance_ratchet(
        &self,
        ratchet_counter_goal: RatchetCounter,
        frame_counter: FrameCounter,
    ) -> Self {
        let mut cur;
        let mut secret;

        if frame_counter > self.ratchet_frame {
            cur = self.sender_state.ratchet_counter;
            secret = self.sender_state.current_secret;
        } else {
            cur = self.old_ratchet_counter;
            secret = self.old_secret;
        }

        while cur != ratchet_counter_goal {
            let secret_hkdf = Hkdf::<Sha256>::new(None, &secret);
            secret_hkdf
                .expand(RATCHET_INFO_STRING, &mut secret[..])
                .unwrap_or_else(|_| {
                    panic!(
                        "HKDF should work with output of length {}",
                        std::mem::size_of::<Secret>()
                    )
                });
            cur = cur.wrapping_add(1);
        }
        let sender_state = SenderState::new(ratchet_counter_goal, secret);
        if frame_counter.wrapping_sub(self.ratchet_frame) > MAX_OOO_FRAMES {
            Self {
                sender_state,
                ratchet_frame: frame_counter,
                old_secret: self.sender_state.current_secret,
                old_ratchet_counter: self.sender_state.ratchet_counter,
            }
        } else {
            Self {
                sender_state,
                ratchet_frame: frame_counter,
                old_secret: self.old_secret,
                old_ratchet_counter: self.old_ratchet_counter,
            }
        }
    }

    /// Advance the old value, if needed, to limit retention of old secrets.
    /// This is not done in try_advance_ratchet to avoid unnecessary work in
    /// case the ratchet secret is not used.
    fn limit_ooo(&mut self) {
        while self
            .sender_state
            .ratchet_counter
            .wrapping_sub(self.old_ratchet_counter)
            > MAX_OOO_RATCHETS
        {
            let secret_hkdf = Hkdf::<Sha256>::new(None, &self.old_secret);
            secret_hkdf
                .expand(RATCHET_INFO_STRING, &mut self.old_secret[..])
                .unwrap_or_else(|_| {
                    panic!(
                        "HKDF should work with output of length {}",
                        std::mem::size_of::<Secret>()
                    )
                });
            self.old_ratchet_counter = self.old_ratchet_counter.wrapping_add(1);
        }
    }
}

fn convert_frame_counter_to_iv(frame_counter: FrameCounter) -> Iv {
    const_assert!(size_of::<Iv>() >= 8);
    let mut result = [0u8; size_of::<Iv>()];
    result[..8].copy_from_slice(&frame_counter.to_be_bytes()[..]);
    result
}

fn check_mac(state: &ReceiverState, frame_counter: FrameCounter, data: &[u8], mac: &Mac) -> bool {
    let iv = convert_frame_counter_to_iv(frame_counter);
    let mut hmac = HmacSha256::new_from_slice(&state.sender_state.current_hmac_key[..])
        .expect("HMAC can take key of any size");
    hmac.update(&iv[..]);
    hmac.update(&len_as_u32_be_bytes(data)[..]);
    hmac.update(data);
    hmac.update(&0_u32.to_be_bytes());
    let hmac_result = hmac.finalize().into_bytes();
    const_assert!(MAC_SIZE_BYTES <= HMAC_SHA256_SIZE_BYTES);
    let result = hmac_result[..MAC_SIZE_BYTES].ct_eq(mac);
    bool::from(result)
}

fn len_as_u32_be_bytes(slice: &[u8]) -> [u8; 4] {
    (slice.len() as u32).to_be_bytes()
}

fn decrypt_internal(state: &ReceiverState, frame_counter: FrameCounter, data: &mut [u8]) {
    let mut cipher = Aes256Ctr::new(
        &state.sender_state.current_aes_key.into(),
        convert_frame_counter_to_iv(frame_counter)[..].into(),
    );
    cipher.apply_keystream(data);
}

pub struct Context {
    sender_state: SenderState,
    next_frame_counter: FrameCounter,
    remote_states_by_id: HashMap<SenderId, Vec<ReceiverState>>,
}

impl Context {
    /// Generates a new RingRTC crypto Context.
    pub fn new(initial_send_secret: Secret) -> Self {
        let sender_state = SenderState::new(0, initial_send_secret);
        Self {
            sender_state,
            next_frame_counter: 1,
            remote_states_by_id: HashMap::new(),
        }
    }

    /// Encrypts a frame of plaintext into a frame of ciphertext.
    ///
    /// This function alters the passed in data slice by applying AES-256-CTR on it.
    /// Additionally, the slice mac is filled in with a sequence of mac bytes to transmit over the
    /// wire with the ciphertext.
    pub fn encrypt(
        &mut self,
        data: &mut [u8],
        mac: &mut Mac,
    ) -> Result<(RatchetCounter, FrameCounter), Error> {
        let frame_counter = self.next_frame_counter;
        self.next_frame_counter += 1;

        let iv = convert_frame_counter_to_iv(frame_counter);
        let mut cipher = Aes256Ctr::new(&self.sender_state.current_aes_key.into(), &iv.into());
        cipher.apply_keystream(data);
        let mut hmac = HmacSha256::new_from_slice(&self.sender_state.current_hmac_key[..])
            .expect("HMAC can take key of any size");
        hmac.update(&iv[..]);
        hmac.update(&len_as_u32_be_bytes(data)[..]);
        hmac.update(data);
        hmac.update(&0_u32.to_be_bytes());
        let hmac_result = hmac.finalize().into_bytes();
        const_assert!(MAC_SIZE_BYTES <= HMAC_SHA256_SIZE_BYTES);
        mac.copy_from_slice(&hmac_result[..MAC_SIZE_BYTES]);
        Ok((self.sender_state.ratchet_counter, frame_counter))
    }

    /// Decrypts a frame of ciphertext into a frame of plaintext.
    ///
    /// This function alters the passed in data slice by applying AES-256-CTR on it.
    pub fn decrypt(
        &mut self,
        sender_id: SenderId,
        ratchet_counter: RatchetCounter,
        frame_counter: FrameCounter,
        data: &mut [u8],
        mac: &Mac,
    ) -> Result<(), Error> {
        let states = self.get_mut_ref_state_vec_by_id(sender_id);

        // try all states with matching ratchet counters first
        for state in states.iter() {
            if state.sender_state.ratchet_counter == ratchet_counter
                && check_mac(state, frame_counter, data, mac)
            {
                decrypt_internal(state, frame_counter, data);
                return Ok(());
            }
        }

        // before giving up, try more expensive repeated ratcheting of each state to match given ratchet counter
        for state in states.iter_mut() {
            let mut try_state = state.try_advance_ratchet(ratchet_counter, frame_counter);
            if check_mac(&try_state, frame_counter, data, mac) {
                try_state.limit_ooo();
                *state = try_state;
                decrypt_internal(state, frame_counter, data);
                return Ok(());
            }
        }

        Err(Error::NoMatchingReceiverState)
    }

    pub fn send_state(&self) -> (RatchetCounter, Secret) {
        (
            self.sender_state.ratchet_counter,
            self.sender_state.current_secret,
        )
    }

    /// Ratchets our send state forward.
    ///
    /// This should be called when a new recipient joins the call. When an existing recipient leaves
    /// the call, [reset_send_ratchet] should be used instead.
    pub fn advance_send_ratchet(&mut self) -> (RatchetCounter, Secret) {
        self.sender_state.mut_advance_ratchet();
        self.send_state()
    }

    /// Commit a send secret and start using it for subsequent encrypt calls.
    pub fn reset_send_ratchet(&mut self, secret: Secret) {
        self.sender_state = SenderState::new(0, secret);
    }

    /// Pushes a new ReceiverState onto the remote sender states map.
    ///
    /// A limited number of historical receiver states are kept for each sender in order to handle
    /// frames delivered out of order with updated secrets.
    pub fn add_receive_secret(
        &mut self,
        sender_id: SenderId,
        ratchet_counter: RatchetCounter,
        secret: Secret,
    ) {
        let states = self.get_mut_ref_state_vec_by_id(sender_id);
        if states.len() == MAX_RECEIVER_STATES_TO_RETAIN {
            states.pop();
        }
        states.insert(0, ReceiverState::new(ratchet_counter, secret));
    }

    fn get_mut_ref_state_vec_by_id(&mut self, sender_id: SenderId) -> &mut Vec<ReceiverState> {
        self.remote_states_by_id
            .entry(sender_id)
            .or_insert_with(|| Vec::with_capacity(MAX_RECEIVER_STATES_TO_RETAIN))
    }
}

#[cfg(test)]
mod tests {
    use rand::prelude::*;

    use super::*;

    #[test]
    fn test_sender_state() {
        let secret: Secret = [
            1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23,
            24, 25, 26, 27, 28, 29, 30, 31, 32,
        ];
        let mut sender_state = SenderState::new(0, secret);
        assert_ne!(AesKey::default(), sender_state.current_aes_key);
        assert_ne!(HmacKey::default(), sender_state.current_hmac_key);
        assert_ne!(sender_state.current_aes_key, sender_state.current_hmac_key);
        assert_eq!(0, sender_state.ratchet_counter);

        let old_aes_key = sender_state.current_aes_key;
        let old_hmac_key = sender_state.current_hmac_key;
        sender_state.mut_advance_ratchet();
        assert_ne!(AesKey::default(), sender_state.current_aes_key);
        assert_ne!(HmacKey::default(), sender_state.current_hmac_key);
        assert_ne!(old_aes_key, sender_state.current_aes_key);
        assert_ne!(old_hmac_key, sender_state.current_hmac_key);
        assert_ne!(sender_state.current_aes_key, sender_state.current_hmac_key);
        assert_eq!(1, sender_state.ratchet_counter);
    }

    #[test]
    fn test_encrypt_decrypt() -> Result<(), Box<dyn std::error::Error>> {
        let plaintext = b"Whan that Aprille with his shoures soote";
        let mut rng = StdRng::from_seed([0x3a; 32]);
        let send_secret = random_secret(&mut rng);
        let mut ctx = Context::new(send_secret);
        let sender_id: SenderId = 42;
        ctx.add_receive_secret(sender_id, 0, send_secret);

        let mut data = plaintext.to_vec();
        let mut mac = Mac::default();
        let (ratchet_counter, frame_counter) = ctx.encrypt(&mut data[..], &mut mac)?;
        assert_eq!(0, ratchet_counter);
        assert_ne!(&plaintext[..], &data[..]);

        ctx.decrypt(
            sender_id,
            ratchet_counter,
            frame_counter,
            &mut data[..],
            &mac,
        )?;
        assert_eq!(&plaintext[..], &data[..]);

        Ok(())
    }

    #[test]
    fn test_ratchet() -> Result<(), Box<dyn std::error::Error>> {
        let plaintext = b"The droghte of March hath perced to the roote";
        let mut rng = StdRng::from_seed([0x42; 32]);
        let send_secret = random_secret(&mut rng);
        let mut ctx = Context::new(send_secret);
        let sender_id: SenderId = 8675309;
        ctx.add_receive_secret(sender_id, 0, send_secret);

        let mut data = plaintext.to_vec();
        let mut mac = Mac::default();
        let (ratchet_counter, frame_counter) = ctx.encrypt(&mut data[..], &mut mac)?;
        assert_eq!(0, ratchet_counter);
        ctx.decrypt(
            sender_id,
            ratchet_counter,
            frame_counter,
            &mut data[..],
            &mac,
        )?;
        assert_eq!(&plaintext[..], &data[..]);

        let (ratchet_counter2, secret2) = ctx.advance_send_ratchet();
        // Another receiver that learned the secret after the ratchet was advanced
        let mut ctx2 = Context::new(random_secret(&mut rng));
        ctx2.add_receive_secret(sender_id, ratchet_counter2, secret2);

        let mut data = plaintext.to_vec();
        let mut mac = [0u8; MAC_SIZE_BYTES];
        let (ratchet_counter, frame_counter) = ctx.encrypt(&mut data[..], &mut mac)?;
        assert_eq!(1, ratchet_counter);
        ctx.decrypt(
            sender_id,
            ratchet_counter,
            frame_counter,
            &mut data[..],
            &mac,
        )?;
        assert_eq!(&plaintext[..], &data[..]);

        let mut data = plaintext.to_vec();
        let (ratchet_counter, frame_counter) = ctx.encrypt(&mut data[..], &mut mac)?;
        assert_eq!(ratchet_counter2, ratchet_counter);
        ctx2.decrypt(
            sender_id,
            ratchet_counter,
            frame_counter,
            &mut data[..],
            &mac,
        )?;
        assert_eq!(&plaintext[..], &data[..]);

        Ok(())
    }

    #[test]
    fn test_rotate_secret() -> Result<(), Box<dyn std::error::Error>> {
        let plaintext = b"And bathed every veyne in swich licour";
        let mut rng = StdRng::from_seed([0x76; 32]);
        let send_secret = random_secret(&mut rng);
        let mut ctx = Context::new(send_secret);
        let sender_id: SenderId = 1392;
        ctx.add_receive_secret(sender_id, 0, send_secret);

        let mut data = plaintext.to_vec();
        let mut mac = Mac::default();
        let (ratchet_counter, frame_counter) = ctx.encrypt(&mut data[..], &mut mac)?;
        assert_eq!(0, ratchet_counter);
        assert_eq!(1, frame_counter);
        ctx.decrypt(
            sender_id,
            ratchet_counter,
            frame_counter,
            &mut data[..],
            &mac,
        )?;
        assert_eq!(&plaintext[..], &data[..]);

        let new_secret = random_secret(&mut rng);
        ctx.add_receive_secret(sender_id, 0, new_secret);

        let mut data = plaintext.to_vec();
        let mut mac = Mac::default();
        let (ratchet_counter, frame_counter) = ctx.encrypt(&mut data[..], &mut mac)?;
        assert_eq!(0, ratchet_counter);
        assert_eq!(2, frame_counter);
        ctx.decrypt(
            sender_id,
            ratchet_counter,
            frame_counter,
            &mut data[..],
            &mac,
        )?;
        assert_eq!(&plaintext[..], &data[..]);

        ctx.reset_send_ratchet(new_secret);

        let mut data = plaintext.to_vec();
        let mut mac = Mac::default();
        let (ratchet_counter, frame_counter) = ctx.encrypt(&mut data[..], &mut mac)?;
        assert_eq!(0, ratchet_counter);
        assert_eq!(3, frame_counter);
        ctx.decrypt(
            sender_id,
            ratchet_counter,
            frame_counter,
            &mut data[..],
            &mac,
        )?;
        assert_eq!(&plaintext[..], &data[..]);

        Ok(())
    }

    #[test]
    fn test_bad_mac() -> Result<(), Box<dyn std::error::Error>> {
        let plaintext = b"Of which vertu engendred is the flour";
        let mut rng = StdRng::from_seed([0x12; 32]);
        let send_secret = random_secret(&mut rng);
        let mut ctx = Context::new(send_secret);
        let sender_id: SenderId = 1492;
        ctx.add_receive_secret(sender_id, 0, send_secret);

        let mut data = plaintext.to_vec();
        let mut mac = Mac::default();
        let (ratchet_counter, frame_counter) = ctx.encrypt(&mut data[..], &mut mac)?;

        mac[0] = mac[0].wrapping_add(1);
        let err = ctx
            .decrypt(
                sender_id,
                ratchet_counter,
                frame_counter,
                &mut data[..],
                &mac,
            )
            .expect_err("decrypt should have returned an error");
        assert_eq!(err, Error::NoMatchingReceiverState);

        mac[0] = mac[0].wrapping_sub(1);
        ctx.decrypt(
            sender_id,
            ratchet_counter,
            frame_counter,
            &mut data[..],
            &mac,
        )?;
        assert_eq!(&plaintext[..], &data[..]);

        Ok(())
    }

    #[test]
    fn test_advance_ratchet_equal_sender_states() {
        let mut rng = StdRng::from_seed([0x34; 32]);
        let secret = random_secret(&mut rng);
        let sender_state = SenderState::new(0, secret);
        let receiver_state = ReceiverState::new(0, secret);
        let mut sender_state_mut = sender_state;
        let receiver_state_adv = receiver_state.try_advance_ratchet(5, 0);
        for _ in 0..5 {
            sender_state_mut.mut_advance_ratchet();
        }
        assert_eq!(receiver_state_adv.sender_state, sender_state_mut);
    }

    #[test]
    fn test_ooo_ratchet() -> Result<(), Box<dyn std::error::Error>> {
        let plaintext = b"Whan Zephirus eek with his sweete breeth";
        let mut rng = StdRng::from_seed([0x2D; 32]);
        let send_secret = random_secret(&mut rng);
        let mut ctx = Context::new(send_secret);
        let sender_id: SenderId = 8675309;
        ctx.add_receive_secret(sender_id, 0, send_secret);

        let mut data1 = plaintext.to_vec();
        let mut mac1 = Mac::default();
        let (ratchet_counter1, frame_counter1) = ctx.encrypt(&mut data1[..], &mut mac1)?;
        assert_eq!(0, ratchet_counter1);

        let (ratchet_counter2, secret2) = ctx.advance_send_ratchet();
        // Another receiver that learned the secret after the ratchet was advanced
        let mut ctx2 = Context::new(random_secret(&mut rng));
        ctx2.add_receive_secret(sender_id, ratchet_counter2, secret2);

        let mut data2 = plaintext.to_vec();
        let mut mac2 = [0u8; MAC_SIZE_BYTES];
        let (ratchet_counter2, frame_counter2) = ctx.encrypt(&mut data2[..], &mut mac2)?;
        assert_eq!(1, ratchet_counter2);
        ctx.decrypt(
            sender_id,
            ratchet_counter2,
            frame_counter2,
            &mut data2[..],
            &mac2,
        )?;
        assert_eq!(&plaintext[..], &data2[..]);

        // Now decrypt the first message, out of order
        ctx.decrypt(
            sender_id,
            ratchet_counter1,
            frame_counter1,
            &mut data1[..],
            &mac1,
        )?;
        assert_eq!(&plaintext[..], &data1[..]);
        Ok(())
    }
}
