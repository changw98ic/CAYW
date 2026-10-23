use crate::protocol::{RemoteError, hash, now_ms, random_id};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{cmp::Ordering, fs, path::Path};

/// Minimal, self-contained big unsigned integer for RSA modular arithmetic.
/// Limbs are stored in little-endian base 2^32 so 64-bit products never overflow.
#[derive(Clone, Debug, PartialEq, Eq)]
struct BigUint {
    limbs: Vec<u32>,
}

impl BigUint {
    fn zero() -> Self {
        Self { limbs: Vec::new() }
    }

    fn one() -> Self {
        Self { limbs: vec![1] }
    }

    fn from_u32(v: u32) -> Self {
        if v == 0 {
            Self::zero()
        } else {
            Self { limbs: vec![v] }
        }
    }

    fn is_zero(&self) -> bool {
        self.limbs.is_empty()
    }

    fn is_even(&self) -> bool {
        self.limbs.first().is_none_or(|&w| w & 1 == 0)
    }

    fn normalize(&mut self) {
        while self.limbs.last() == Some(&0) {
            self.limbs.pop();
        }
    }

    fn bit_len(&self) -> usize {
        let Some(&last) = self.limbs.last() else {
            return 0;
        };
        (self.limbs.len() - 1) * 32 + (32 - last.leading_zeros() as usize)
    }

    fn get_bit(&self, idx: usize) -> bool {
        let word = idx / 32;
        let bit = idx % 32;
        self.limbs.get(word).is_some_and(|&w| ((w >> bit) & 1) != 0)
    }

    fn set_bit(&mut self, idx: usize) {
        let word = idx / 32;
        let bit = idx % 32;
        if self.limbs.len() <= word {
            self.limbs.resize(word + 1, 0);
        }
        self.limbs[word] |= 1u32 << bit;
    }

    fn from_bytes_be(bytes: &[u8]) -> Self {
        let mut limbs = Vec::with_capacity(bytes.len().div_ceil(4));
        let mut i = bytes.len();
        while i > 0 {
            let start = i.saturating_sub(4);
            let mut word = 0u32;
            for &b in &bytes[start..i] {
                word = (word << 8) | u32::from(b);
            }
            limbs.push(word);
            i = start;
        }
        let mut out = Self { limbs };
        out.normalize();
        out
    }

    fn to_bytes_be(&self) -> Vec<u8> {
        if self.is_zero() {
            return vec![0];
        }
        let mut out = Vec::with_capacity(self.limbs.len() * 4);
        for &w in self.limbs.iter().rev() {
            out.extend_from_slice(&w.to_be_bytes());
        }
        let first_non_zero = out.iter().position(|&b| b != 0).unwrap_or(out.len() - 1);
        out[first_non_zero..].to_vec()
    }

    fn to_bytes_be_padded(&self, len: usize) -> Vec<u8> {
        let raw = if self.is_zero() {
            Vec::new()
        } else {
            self.to_bytes_be()
        };
        if raw.len() >= len {
            raw
        } else {
            let mut padded = vec![0u8; len - raw.len()];
            padded.extend_from_slice(&raw);
            padded
        }
    }

    fn to_hex(&self) -> String {
        self.to_bytes_be()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn from_hex(hex: &str) -> Result<Self, RemoteError> {
        let trimmed = hex.trim();
        if trimmed.is_empty() || !trimmed.len().is_multiple_of(2) {
            return Err(RemoteError::InvalidRequest);
        }
        let mut bytes = Vec::with_capacity(trimmed.len() / 2);
        for i in (0..trimmed.len()).step_by(2) {
            let b = u8::from_str_radix(&trimmed[i..i + 2], 16)
                .map_err(|_| RemoteError::InvalidRequest)?;
            bytes.push(b);
        }
        Ok(Self::from_bytes_be(&bytes))
    }

    fn cmp(&self, other: &Self) -> Ordering {
        match self.limbs.len().cmp(&other.limbs.len()) {
            Ordering::Equal => {
                for (&a, &b) in self.limbs.iter().rev().zip(other.limbs.iter().rev()) {
                    match a.cmp(&b) {
                        Ordering::Equal => continue,
                        ord => return ord,
                    }
                }
                Ordering::Equal
            }
            ord => ord,
        }
    }

    fn add(&self, other: &Self) -> Self {
        let max_len = self.limbs.len().max(other.limbs.len());
        let mut limbs = Vec::with_capacity(max_len + 1);
        let mut carry = 0u64;
        for i in 0..max_len {
            let a = u64::from(self.limbs.get(i).copied().unwrap_or(0));
            let b = u64::from(other.limbs.get(i).copied().unwrap_or(0));
            let sum = a + b + carry;
            limbs.push(sum as u32);
            carry = sum >> 32;
        }
        if carry > 0 {
            limbs.push(carry as u32);
        }
        let mut out = Self { limbs };
        out.normalize();
        out
    }

    fn sub(&self, other: &Self) -> Self {
        debug_assert!(self.cmp(other) != Ordering::Less);
        let mut limbs = Vec::with_capacity(self.limbs.len());
        let mut borrow = 0i64;
        for i in 0..self.limbs.len() {
            let a = i64::from(self.limbs[i]);
            let b = i64::from(other.limbs.get(i).copied().unwrap_or(0));
            let mut diff = a - b - borrow;
            if diff < 0 {
                diff += 1i64 << 32;
                borrow = 1;
            } else {
                borrow = 0;
            }
            limbs.push(diff as u32);
        }
        let mut out = Self { limbs };
        out.normalize();
        out
    }

    fn mul(&self, other: &Self) -> Self {
        if self.is_zero() || other.is_zero() {
            return Self::zero();
        }
        let mut limbs = vec![0u32; self.limbs.len() + other.limbs.len()];
        for (i, &a) in self.limbs.iter().enumerate() {
            let mut carry = 0u64;
            for (j, &b) in other.limbs.iter().enumerate() {
                let prod = u64::from(a) * u64::from(b) + u64::from(limbs[i + j]) + carry;
                limbs[i + j] = prod as u32;
                carry = prod >> 32;
            }
            limbs[i + other.limbs.len()] = carry as u32;
        }
        let mut out = Self { limbs };
        out.normalize();
        out
    }

    fn shl1(&mut self) {
        let mut carry = 0u32;
        for w in &mut self.limbs {
            let next = *w >> 31;
            *w = (*w << 1) | carry;
            carry = next;
        }
        if carry != 0 {
            self.limbs.push(carry);
        }
    }

    fn shr1(&mut self) {
        let mut carry = 0u32;
        for w in self.limbs.iter_mut().rev() {
            let next = *w & 1;
            *w = (*w >> 1) | (carry << 31);
            carry = next;
        }
        self.normalize();
    }

    fn div_rem(&self, divisor: &Self) -> (Self, Self) {
        assert!(!divisor.is_zero());
        if self.cmp(divisor) == Ordering::Less {
            return (Self::zero(), self.clone());
        }
        let mut q = Self::zero();
        let mut r = Self::zero();
        for bit in (0..self.bit_len()).rev() {
            r.shl1();
            if self.get_bit(bit) {
                if r.limbs.is_empty() {
                    r.limbs.push(1);
                } else {
                    r.limbs[0] |= 1;
                }
            }
            if r.cmp(divisor) != Ordering::Less {
                r = r.sub(divisor);
                q.set_bit(bit);
            }
        }
        q.normalize();
        r.normalize();
        (q, r)
    }

    fn rem(&self, modulus: &Self) -> Self {
        self.div_rem(modulus).1
    }

    fn mod_mul(&self, other: &Self, modulus: &Self) -> Self {
        self.mul(other).rem(modulus)
    }

    fn mod_pow(&self, exp: &Self, modulus: &Self) -> Self {
        if modulus == &Self::one() {
            return Self::zero();
        }
        let mut result = Self::one();
        let mut base = self.rem(modulus);
        let bits = exp.bit_len();
        for i in 0..bits {
            if exp.get_bit(i) {
                result = result.mod_mul(&base, modulus);
            }
            if i + 1 < bits {
                base = base.mod_mul(&base, modulus);
            }
        }
        result
    }

    fn gcd(mut a: Self, mut b: Self) -> Self {
        while !b.is_zero() {
            let r = a.rem(&b);
            a = b;
            b = r;
        }
        a
    }

    /// Computes `a^-1 mod m` using the unsigned extended Euclidean algorithm.
    fn mod_inverse(a: &Self, m: &Self) -> Option<Self> {
        let mut t = (Self::zero(), false);
        let mut new_t = (Self::one(), false);
        let mut r = m.clone();
        let mut new_r = a.rem(m);

        while !new_r.is_zero() {
            let (q, rem_r) = r.div_rem(&new_r);
            r = new_r;
            new_r = rem_r;

            let q_nt = q.mul(&new_t.0);
            let next_t = if t.1 == new_t.1 {
                if t.0.cmp(&q_nt) != Ordering::Less {
                    (t.0.sub(&q_nt), t.1)
                } else {
                    (q_nt.sub(&t.0), !t.1)
                }
            } else {
                (t.0.add(&q_nt), t.1)
            };
            t = new_t;
            new_t = next_t;
        }
        if r != Self::one() {
            return None;
        }
        let reduced = t.0.rem(m);
        if t.1 && !reduced.is_zero() {
            Some(m.sub(&reduced))
        } else {
            Some(reduced)
        }
    }
}

const SMALL_PRIMES: &[u32] = &[
    3, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37, 41, 43, 47, 53, 59, 61, 67, 71, 73, 79, 83, 89, 97,
    101, 103, 107, 109, 113, 127, 131, 137, 139, 149, 151, 157, 163, 167, 173, 179, 181, 191, 193,
    197, 199, 211, 223, 227, 229, 233, 239, 241, 251,
];

fn is_probable_prime(n: &BigUint) -> bool {
    if n.is_even() {
        return false;
    }
    for &p in SMALL_PRIMES {
        let pv = BigUint::from_u32(p);
        if n == &pv {
            return true;
        }
        if n.rem(&pv).is_zero() {
            return false;
        }
    }
    let one = BigUint::one();
    let n_minus_one = n.sub(&one);
    let mut d = n_minus_one.clone();
    let mut s = 0usize;
    while d.is_even() {
        d.shr1();
        s += 1;
    }
    for &witness in &[2u32, 3, 5, 7, 11, 13, 17] {
        let a = BigUint::from_u32(witness);
        if a.cmp(&n_minus_one) != Ordering::Less {
            continue;
        }
        let mut x = a.mod_pow(&d, n);
        if x == one || x == n_minus_one {
            continue;
        }
        let mut composite = true;
        for _ in 1..s {
            x = x.mod_mul(&x, n);
            if x == n_minus_one {
                composite = false;
                break;
            }
        }
        if composite {
            return false;
        }
    }
    true
}

fn generate_prime(bits: usize) -> Result<BigUint, RemoteError> {
    let byte_len = bits.div_ceil(8);
    let mut buf = vec![0u8; byte_len];
    getrandom::fill(&mut buf).map_err(|_| RemoteError::Configuration)?;
    buf[0] |= 0xC0;
    buf[byte_len - 1] |= 0x01;
    let mut candidate = BigUint::from_bytes_be(&buf);
    let two = BigUint::from_u32(2);
    let e = BigUint::from_u32(65537);
    loop {
        if is_probable_prime(&candidate) {
            let p_minus_one = candidate.sub(&BigUint::one());
            if BigUint::gcd(p_minus_one, e.clone()) == BigUint::one() {
                return Ok(candidate);
            }
        }
        candidate = candidate.add(&two);
    }
}

/// Public RSA key exchanged once during Server-Device binding.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RsaPublicKey {
    pub n_hex: String,
    pub e_hex: String,
}

/// Full RSA keypair persisted on the Device or Server after one-time binding.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RsaKeyPair {
    pub n_hex: String,
    pub e_hex: String,
    pub d_hex: String,
    pub p_hex: String,
    pub q_hex: String,
}

impl std::fmt::Debug for RsaKeyPair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RsaKeyPair")
            .field("n_hex", &self.n_hex)
            .field("e_hex", &self.e_hex)
            .field("private_key", &"[redacted]")
            .finish()
    }
}

impl RsaPublicKey {
    pub fn fingerprint(&self) -> String {
        let digest = hash(format!("rsa:{}:{}", self.e_hex, self.n_hex).as_bytes());
        format!("SHA256:{}", &digest[..32])
    }

    pub fn to_pem(&self) -> String {
        let raw = format!("RSA-PUB:{}:{}", self.e_hex, self.n_hex);
        let b64 = hex_to_base64_lines(raw.as_bytes());
        format!("-----BEGIN RSA PUBLIC KEY-----\n{b64}\n-----END RSA PUBLIC KEY-----")
    }

    pub fn modulus_bytes(&self) -> Result<usize, RemoteError> {
        let n = BigUint::from_hex(&self.n_hex)?;
        let bytes = n.bit_len().div_ceil(8);
        if bytes < 48 {
            return Err(RemoteError::InvalidRequest);
        }
        Ok(bytes)
    }

    /// Encrypts an arbitrary-length payload using RSA-protected session key +
    /// authenticated stream encryption (SHA-256 CTR + HMAC-SHA256).
    pub fn encrypt(&self, plaintext: &[u8]) -> Result<String, RemoteError> {
        let n = BigUint::from_hex(&self.n_hex)?;
        let e = BigUint::from_hex(&self.e_hex)?;
        let k_len = self.modulus_bytes()?;
        let mut session_key = [0u8; 32];
        let mut nonce = [0u8; 16];
        getrandom::fill(&mut session_key).map_err(|_| RemoteError::Configuration)?;
        getrandom::fill(&mut nonce).map_err(|_| RemoteError::Configuration)?;

        // PKCS#1 v1.5 type 2 block: 0x00 || 0x02 || non-zero pad || 0x00 || session_key(32)
        let pad_len = k_len
            .checked_sub(3 + session_key.len())
            .ok_or(RemoteError::InvalidRequest)?;
        if pad_len < 8 {
            return Err(RemoteError::InvalidRequest);
        }
        let mut block = Vec::with_capacity(k_len);
        block.push(0x00);
        block.push(0x02);
        let mut pad = vec![0u8; pad_len];
        getrandom::fill(&mut pad).map_err(|_| RemoteError::Configuration)?;
        for b in &mut pad {
            if *b == 0 {
                *b = 0x5A;
            }
        }
        block.extend_from_slice(&pad);
        block.push(0x00);
        block.extend_from_slice(&session_key);

        let m = BigUint::from_bytes_be(&block);
        let c = m.mod_pow(&e, &n);
        let rsa_cipher_hex = c
            .to_bytes_be_padded(k_len)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();

        let stream_cipher = xor_sha256_stream(&session_key, &nonce, plaintext);
        let cipher_hex = stream_cipher
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let nonce_hex = nonce.iter().map(|b| format!("{b:02x}")).collect::<String>();
        let mac = compute_mac(&session_key, &rsa_cipher_hex, &nonce_hex, &cipher_hex);
        Ok(format!(
            "v1.{rsa_cipher_hex}.{nonce_hex}.{cipher_hex}.{mac}"
        ))
    }

    /// Verifies an RSA-SHA256 signature created by the matching `RsaKeyPair`.
    pub fn verify_sha256(&self, message: &[u8], signature_hex: &str) -> bool {
        let Ok(n) = BigUint::from_hex(&self.n_hex) else {
            return false;
        };
        let Ok(e) = BigUint::from_hex(&self.e_hex) else {
            return false;
        };
        let Ok(k_len) = self.modulus_bytes() else {
            return false;
        };
        let Ok(sig) = BigUint::from_hex(signature_hex) else {
            return false;
        };
        if sig.cmp(&n) != Ordering::Less {
            return false;
        }
        let recovered = sig.mod_pow(&e, &n).to_bytes_be_padded(k_len);
        let Some(expected) = pkcs1_sign_pad(message, k_len) else {
            return false;
        };
        constant_time_eq(&recovered, &expected)
    }
}

impl RsaKeyPair {
    /// Generates a fresh RSA keypair (`bits` defaults to 512 in fast local mode or 1024).
    pub fn generate() -> Result<Self, RemoteError> {
        Self::generate_with_bits(512)
    }

    pub fn generate_with_bits(bits: usize) -> Result<Self, RemoteError> {
        let half = bits.max(384) / 2;
        let e = BigUint::from_u32(65537);
        loop {
            let p = generate_prime(half)?;
            let q = generate_prime(half)?;
            if p == q {
                continue;
            }
            let n = p.mul(&q);
            let p1 = p.sub(&BigUint::one());
            let q1 = q.sub(&BigUint::one());
            let phi = p1.mul(&q1);
            if let Some(d) = BigUint::mod_inverse(&e, &phi) {
                return Ok(Self {
                    n_hex: n.to_hex(),
                    e_hex: e.to_hex(),
                    d_hex: d.to_hex(),
                    p_hex: p.to_hex(),
                    q_hex: q.to_hex(),
                });
            }
        }
    }

    pub fn public_key(&self) -> RsaPublicKey {
        RsaPublicKey {
            n_hex: self.n_hex.clone(),
            e_hex: self.e_hex.clone(),
        }
    }

    fn private_pow(&self, c: &BigUint) -> Result<BigUint, RemoteError> {
        let n = BigUint::from_hex(&self.n_hex)?;
        let d = BigUint::from_hex(&self.d_hex)?;
        if let (Ok(p), Ok(q)) = (
            BigUint::from_hex(&self.p_hex),
            BigUint::from_hex(&self.q_hex),
        ) && !p.is_zero()
            && !q.is_zero()
        {
            let p1 = p.sub(&BigUint::one());
            let q1 = q.sub(&BigUint::one());
            if let Some(qinv) = BigUint::mod_inverse(&q, &p) {
                let dp = d.rem(&p1);
                let dq = d.rem(&q1);
                let m1 = c.rem(&p).mod_pow(&dp, &p);
                let m2 = c.rem(&q).mod_pow(&dq, &q);
                let diff = if m1.cmp(&m2) != Ordering::Less {
                    m1.sub(&m2)
                } else {
                    let m2_mod_p = m2.rem(&p);
                    p.add(&m1).sub(&m2_mod_p)
                };
                let h = qinv.mod_mul(&diff, &p);
                return Ok(m2.add(&h.mul(&q)).rem(&n));
            }
        }
        Ok(c.mod_pow(&d, &n))
    }

    /// Decrypts a payload encrypted with `RsaPublicKey::encrypt`.
    pub fn decrypt(&self, envelope: &str) -> Result<Vec<u8>, RemoteError> {
        let parts: Vec<&str> = envelope.split('.').collect();
        if parts.len() != 5 || parts[0] != "v1" {
            return Err(RemoteError::Unauthorized);
        }
        let (rsa_cipher_hex, nonce_hex, cipher_hex, mac) = (parts[1], parts[2], parts[3], parts[4]);
        let k_len = self.public_key().modulus_bytes()?;
        let c = BigUint::from_hex(rsa_cipher_hex)?;
        let n = BigUint::from_hex(&self.n_hex)?;
        if c.cmp(&n) != Ordering::Less {
            return Err(RemoteError::Unauthorized);
        }
        let m = self.private_pow(&c)?;
        let block = m.to_bytes_be_padded(k_len);
        if block.len() < 11 || block[0] != 0x00 || block[1] != 0x02 {
            return Err(RemoteError::Unauthorized);
        }
        let Some(sep) = block[2..].iter().position(|&b| b == 0x00).map(|i| i + 2) else {
            return Err(RemoteError::Unauthorized);
        };
        if sep < 10 || block.len() - (sep + 1) != 32 {
            return Err(RemoteError::Unauthorized);
        }
        let mut session_key = [0u8; 32];
        session_key.copy_from_slice(&block[sep + 1..]);
        let expected_mac = compute_mac(&session_key, rsa_cipher_hex, nonce_hex, cipher_hex);
        if !constant_time_eq(mac.as_bytes(), expected_mac.as_bytes()) {
            return Err(RemoteError::Unauthorized);
        }
        let nonce_bytes = decode_hex(nonce_hex)?;
        if nonce_bytes.len() != 16 {
            return Err(RemoteError::Unauthorized);
        }
        let mut nonce = [0u8; 16];
        nonce.copy_from_slice(&nonce_bytes);
        let cipher_bytes = decode_hex(cipher_hex)?;
        Ok(xor_sha256_stream(&session_key, &nonce, &cipher_bytes))
    }

    /// Signs a message with RSA-SHA256.
    pub fn sign_sha256(&self, message: &[u8]) -> Result<String, RemoteError> {
        let k_len = self.public_key().modulus_bytes()?;
        let padded = pkcs1_sign_pad(message, k_len).ok_or(RemoteError::InvalidRequest)?;
        let m = BigUint::from_bytes_be(&padded);
        let sig = self.private_pow(&m)?;
        Ok(sig
            .to_bytes_be_padded(k_len)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect())
    }
}

fn pkcs1_sign_pad(message: &[u8], k_len: usize) -> Option<Vec<u8>> {
    let digest = Sha256::digest(message);
    if k_len < 3 + 8 + digest.len() {
        return None;
    }
    let ff_len = k_len - 3 - digest.len();
    let mut out = Vec::with_capacity(k_len);
    out.push(0x00);
    out.push(0x01);
    out.resize(2 + ff_len, 0xFF);
    out.push(0x00);
    out.extend_from_slice(&digest);
    Some(out)
}

fn xor_sha256_stream(key: &[u8; 32], nonce: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    for (counter, chunk) in data.chunks(32).enumerate() {
        let mut hasher = Sha256::new();
        hasher.update(b"cayw-rsa-stream-v1");
        hasher.update(key);
        hasher.update(nonce);
        hasher.update((counter as u64).to_be_bytes());
        let ks = hasher.finalize();
        for (&b, &k) in chunk.iter().zip(ks.iter()) {
            out.push(b ^ k);
        }
    }
    out
}

fn compute_mac(key: &[u8; 32], rsa_hex: &str, nonce_hex: &str, cipher_hex: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"cayw-rsa-mac-v1:");
    hasher.update(key);
    hasher.update(b":");
    hasher.update(rsa_hex.as_bytes());
    hasher.update(b":");
    hasher.update(nonce_hex.as_bytes());
    hasher.update(b":");
    hasher.update(cipher_hex.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn decode_hex(hex: &str) -> Result<Vec<u8>, RemoteError> {
    if !hex.len().is_multiple_of(2) {
        return Err(RemoteError::InvalidRequest);
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    for i in (0..hex.len()).step_by(2) {
        out.push(u8::from_str_radix(&hex[i..i + 2], 16).map_err(|_| RemoteError::InvalidRequest)?);
    }
    Ok(out)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= usize::from(x ^ y);
    }
    diff == 0
}

fn hex_to_base64_lines(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = u32::from(chunk.get(1).copied().unwrap_or(0));
        let b2 = u32::from(chunk.get(2).copied().unwrap_or(0));
        let triple = (b0 << 16) | (b1 << 8) | b2;
        encoded.push(ALPHABET[((triple >> 18) & 0x3F) as usize] as char);
        encoded.push(ALPHABET[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            encoded.push(ALPHABET[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            encoded.push('=');
        }
        if chunk.len() > 2 {
            encoded.push(ALPHABET[(triple & 0x3F) as usize] as char);
        } else {
            encoded.push('=');
        }
    }
    encoded
        .as_bytes()
        .chunks(64)
        .map(|line| std::str::from_utf8(line).unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
}

/// One-time persistent RSA binding between a Device and a Server.
/// Once bound and saved to disk, the Device and Server establish RSA-encrypted
/// connections automatically forever without requiring tokens or manual re-pairing.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersistentRsaDeviceBinding {
    pub device_id: String,
    pub server_url: String,
    pub device_keypair: RsaKeyPair,
    pub server_public_key: RsaPublicKey,
    pub bound_at_ms: i64,
}

impl PersistentRsaDeviceBinding {
    /// Loads an existing permanent binding from `path`, or performs the one-time
    /// RSA key exchange via `bind_once` and persists it so future starts never re-bind.
    pub fn load_or_bind_once<F>(
        path: &Path,
        server_url: &str,
        bind_once: F,
    ) -> Result<(Self, bool), RemoteError>
    where
        F: FnOnce(&RsaPublicKey) -> Result<(String, RsaPublicKey), RemoteError>,
    {
        if path.exists() {
            let raw = fs::read_to_string(path).map_err(|_| RemoteError::Storage)?;
            let existing: Self = serde_json::from_str(&raw).map_err(|_| RemoteError::Storage)?;
            return Ok((existing, true));
        }
        let device_keypair = RsaKeyPair::generate()?;
        let device_pub = device_keypair.public_key();
        let (device_id, server_public_key) = bind_once(&device_pub)?;
        let binding = Self {
            device_id,
            server_url: server_url.to_owned(),
            device_keypair,
            server_public_key,
            bound_at_ms: now_ms()?,
        };
        binding.save(path)?;
        Ok((binding, false))
    }

    pub fn save(&self, path: &Path) -> Result<(), RemoteError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|_| RemoteError::Storage)?;
        }
        let json = serde_json::to_string_pretty(self).map_err(|_| RemoteError::Storage)?;
        fs::write(path, json).map_err(|_| RemoteError::Storage)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    /// Encrypts a message from Device to Server using Server's RSA public key and
    /// signs it with Device's RSA private key.
    pub fn seal_for_server(&self, payload: &[u8]) -> Result<RsaSealedFrame, RemoteError> {
        let ciphertext = self.server_public_key.encrypt(payload)?;
        let signature = self
            .device_keypair
            .sign_sha256(format!("{}:{}", self.device_id, ciphertext).as_bytes())?;
        Ok(RsaSealedFrame {
            device_id: self.device_id.clone(),
            ciphertext,
            signature,
        })
    }

    /// Verifies Server's RSA signature and decrypts a message sent to this Device.
    pub fn open_from_server(&self, frame: &RsaSealedFrame) -> Result<Vec<u8>, RemoteError> {
        if frame.device_id != self.device_id {
            return Err(RemoteError::Unauthorized);
        }
        if !self.server_public_key.verify_sha256(
            format!("{}:{}", frame.device_id, frame.ciphertext).as_bytes(),
            &frame.signature,
        ) {
            return Err(RemoteError::Unauthorized);
        }
        self.device_keypair.decrypt(&frame.ciphertext)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RsaSealedFrame {
    pub device_id: String,
    pub ciphertext: String,
    pub signature: String,
}

/// Default Web <-> Server login account credentials (`admin` / `admin` initially),
/// with support for changing username and password after logging in.
pub const DEFAULT_SERVER_USERNAME: &str = "admin";
pub const DEFAULT_SERVER_PASSWORD: &str = "admin";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServerAccountRecord {
    pub username: String,
    pub password_salt: String,
    pub password_hash: String,
    pub is_default_credentials: bool,
    pub updated_at_ms: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServerAccountStatus {
    pub username: String,
    pub is_default_credentials: bool,
    pub updated_at_ms: i64,
}

impl ServerAccountRecord {
    pub fn default_account() -> Result<Self, RemoteError> {
        let salt = random_id()?;
        let password_hash = Self::hash_password(&salt, DEFAULT_SERVER_PASSWORD);
        Ok(Self {
            username: DEFAULT_SERVER_USERNAME.to_owned(),
            password_salt: salt,
            password_hash,
            is_default_credentials: true,
            updated_at_ms: now_ms()?,
        })
    }

    pub fn load_or_default(path: &Path) -> Result<Self, RemoteError> {
        if path.exists()
            && let Ok(raw) = fs::read_to_string(path)
            && let Ok(rec) = serde_json::from_str::<Self>(&raw)
        {
            return Ok(rec);
        }
        let default_rec = Self::default_account()?;
        let _ = default_rec.save(path);
        Ok(default_rec)
    }

    pub fn save(&self, path: &Path) -> Result<(), RemoteError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|_| RemoteError::Storage)?;
        }
        let json = serde_json::to_string_pretty(self).map_err(|_| RemoteError::Storage)?;
        fs::write(path, json).map_err(|_| RemoteError::Storage)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    pub fn hash_password(salt: &str, password: &str) -> String {
        hash(format!("cayw-server-account-v1:{salt}:{password}").as_bytes())
    }

    pub fn verify(&self, username: &str, password: &str) -> bool {
        if self.username != username.trim() {
            return false;
        }
        let candidate = Self::hash_password(&self.password_salt, password);
        constant_time_eq(self.password_hash.as_bytes(), candidate.as_bytes())
    }

    pub fn update_credentials(
        &mut self,
        current_password: &str,
        new_username: &str,
        new_password: &str,
    ) -> Result<ServerAccountStatus, RemoteError> {
        let expected = Self::hash_password(&self.password_salt, current_password);
        if !constant_time_eq(self.password_hash.as_bytes(), expected.as_bytes()) {
            return Err(RemoteError::Unauthorized);
        }
        let trimmed_user = new_username.trim();
        if trimmed_user.is_empty()
            || trimmed_user.len() > 64
            || trimmed_user.chars().any(char::is_control)
            || new_password.is_empty()
            || new_password.len() > 128
            || new_password.chars().any(char::is_control)
        {
            return Err(RemoteError::InvalidRequest);
        }
        let new_salt = random_id()?;
        self.username = trimmed_user.to_owned();
        self.password_salt = new_salt.clone();
        self.password_hash = Self::hash_password(&new_salt, new_password);
        self.is_default_credentials =
            self.username == DEFAULT_SERVER_USERNAME && new_password == DEFAULT_SERVER_PASSWORD;
        self.updated_at_ms = now_ms()?;
        Ok(self.status())
    }

    pub fn status(&self) -> ServerAccountStatus {
        ServerAccountStatus {
            username: self.username.clone(),
            is_default_credentials: self.is_default_credentials,
            updated_at_ms: self.updated_at_ms,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rsa_encrypt_decrypt_and_sign_verify_round_trip() {
        let kp = RsaKeyPair::generate().unwrap();
        let pub_key = kp.public_key();
        let msg = b"CAYW one-time RSA bound device-server channel payload";
        let cipher = pub_key.encrypt(msg).unwrap();
        let plain = kp.decrypt(&cipher).unwrap();
        assert_eq!(plain, msg);

        let sig = kp.sign_sha256(msg).unwrap();
        assert!(pub_key.verify_sha256(msg, &sig));
        assert!(!pub_key.verify_sha256(b"tampered", &sig));
    }

    #[test]
    fn persistent_device_binding_binds_once_and_reuses_saved_rsa_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("device-rsa-binding.json");
        let server_kp = RsaKeyPair::generate().unwrap();
        let server_pub = server_kp.public_key();

        let mut bind_calls = 0usize;
        let (b1, already_bound_1) =
            PersistentRsaDeviceBinding::load_or_bind_once(&path, "wss://relay.cayw.local", |_| {
                bind_calls += 1;
                Ok(("dev-rsa-001".to_owned(), server_pub.clone()))
            })
            .unwrap();
        assert!(!already_bound_1);
        assert_eq!(bind_calls, 1);

        // Second start loads from disk without calling bind_once again!
        let (b2, already_bound_2) =
            PersistentRsaDeviceBinding::load_or_bind_once(&path, "wss://relay.cayw.local", |_| {
                panic!("must never re-bind once bound");
            })
            .unwrap();
        assert!(already_bound_2);
        assert_eq!(b1, b2);

        // Verify mutual RSA sealed frame encryption between Device and Server.
        let frame = b2.seal_for_server(b"execute_task_42").unwrap();
        assert!(b2.device_keypair.public_key().verify_sha256(
            format!("{}:{}", frame.device_id, frame.ciphertext).as_bytes(),
            &frame.signature
        ));
        let decrypted_by_server = server_kp.decrypt(&frame.ciphertext).unwrap();
        assert_eq!(decrypted_by_server, b"execute_task_42");
    }

    #[test]
    fn server_default_account_login_and_password_modification() {
        let mut acct = ServerAccountRecord::default_account().unwrap();
        assert!(acct.is_default_credentials);
        assert!(acct.verify("admin", "admin"));
        assert!(!acct.verify("admin", "wrong"));

        assert!(
            acct.update_credentials("wrong", "operator", "newpass123")
                .is_err()
        );
        let updated = acct
            .update_credentials("admin", "operator", "newpass123")
            .unwrap();
        assert!(!updated.is_default_credentials);
        assert_eq!(updated.username, "operator");
        assert!(!acct.verify("admin", "admin"));
        assert!(acct.verify("operator", "newpass123"));
    }
}
