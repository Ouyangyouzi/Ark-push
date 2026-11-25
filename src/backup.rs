use base64::Engine;
use hmac::{Hmac, Mac};
use rand::RngCore;
use serde_json::{json, Value};
use sha2::Sha256;
use subtle::ConstantTimeEq;

const KIND: &str = "arkpan-push-backup-enc";
const ALG: &str = "hmac-sha256-ctr";
const KDF: &str = "pbkdf2-sha256";
const ITERATIONS: u32 = 210_000;
const MIN_ITER: u32 = 10_000;
const MAX_ITER: u32 = 5_000_000;
const MAC_PREFIX: &[u8] = b"arkpan-backup-v1";

#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    #[error("{0}")]
    Message(String),
}

fn b64e(raw: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(raw)
}

fn b64d(text: &str) -> Result<Vec<u8>, BackupError> {
    base64::engine::general_purpose::STANDARD
        .decode(text)
        .map_err(|_| BackupError::Message("备份文件格式损坏".into()))
}

fn derive(password: &str, salt: &[u8], iterations: u32) -> ([u8; 32], [u8; 32]) {
    let mut raw = [0u8; 64];
    pbkdf2::pbkdf2_hmac::<Sha256>(password.as_bytes(), salt, iterations, &mut raw);
    let mut enc = [0u8; 32];
    let mut mac = [0u8; 32];
    enc.copy_from_slice(&raw[..32]);
    mac.copy_from_slice(&raw[32..]);
    (enc, mac)
}

fn xor_stream(key: &[u8; 32], nonce: &[u8], data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut counter = 0u64;
    let mut pos = 0;
    while pos < data.len() {
        let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("hmac key");
        mac.update(nonce);
        mac.update(&counter.to_be_bytes());
        let block = mac.finalize().into_bytes();
        let n = (data.len() - pos).min(block.len());
        for i in 0..n {
            out.push(data[pos + i] ^ block[i]);
        }
        pos += n;
        counter += 1;
    }
    out
}

fn auth_tag(mac_key: &[u8; 32], nonce: &[u8], cipher: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(mac_key).expect("hmac key");
    mac.update(MAC_PREFIX);
    mac.update(nonce);
    mac.update(cipher);
    mac.finalize().into_bytes().to_vec()
}

pub fn encrypt(payload: &Value, password: &str) -> Result<Value, BackupError> {
    if password.is_empty() {
        return Err(BackupError::Message("请设置备份密码".into()));
    }
    let mut salt = [0u8; 16];
    let mut nonce = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut salt);
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    let (enc_key, mac_key) = derive(password, &salt, ITERATIONS);
    let plain = serde_json::to_vec(payload).map_err(|e| BackupError::Message(e.to_string()))?;
    let cipher = xor_stream(&enc_key, &nonce, &plain);
    Ok(json!({
        "kind": KIND,
        "version": 1,
        "alg": ALG,
        "kdf": KDF,
        "iterations": ITERATIONS,
        "salt": b64e(&salt),
        "nonce": b64e(&nonce),
        "tag": b64e(&auth_tag(&mac_key, &nonce, &cipher)),
        "data": b64e(&cipher),
    }))
}

pub fn is_encrypted(v: &Value) -> bool {
    v.get("kind").and_then(Value::as_str) == Some(KIND)
}

pub fn decrypt(envelope: &Value, password: &str) -> Result<Value, BackupError> {
    if !is_encrypted(envelope) {
        return Err(BackupError::Message("不是加密的备份文件".into()));
    }
    if password.is_empty() {
        return Err(BackupError::Message("请输入备份密码".into()));
    }
    if envelope.get("alg").and_then(Value::as_str).unwrap_or(ALG) != ALG {
        return Err(BackupError::Message("不支持的加密算法".into()));
    }
    let iterations = envelope
        .get("iterations")
        .and_then(Value::as_i64)
        .unwrap_or(ITERATIONS as i64) as u32;
    if !(MIN_ITER..=MAX_ITER).contains(&iterations) {
        return Err(BackupError::Message("备份文件的迭代次数不合理".into()));
    }
    let salt = b64d(envelope.get("salt").and_then(Value::as_str).unwrap_or(""))?;
    let nonce = b64d(envelope.get("nonce").and_then(Value::as_str).unwrap_or(""))?;
    let cipher = b64d(envelope.get("data").and_then(Value::as_str).unwrap_or(""))?;
    let tag = b64d(envelope.get("tag").and_then(Value::as_str).unwrap_or(""))?;
    if nonce.len() != 16 || tag.len() != 32 || salt.len() < 8 {
        return Err(BackupError::Message("备份文件格式损坏".into()));
    }
    let (enc_key, mac_key) = derive(password, &salt, iterations);
    let expect = auth_tag(&mac_key, &nonce, &cipher);
    if expect.len() != tag.len() || expect.ct_eq(&tag).unwrap_u8() != 1 {
        return Err(BackupError::Message("密码不正确或备份文件已损坏".into()));
    }
    let plain = xor_stream(&enc_key, &nonce, &cipher);
    let value: Value = serde_json::from_slice(&plain)
        .map_err(|_| BackupError::Message("备份内容解析失败".into()))?;
    if !value.is_object() {
        return Err(BackupError::Message("备份内容格式不正确".into()));
    }
    Ok(value)
}
