//! 本地 CA：生成/加载根证书、为下游域名动态签发叶子证书、安装/卸载到用户证书存储。
//!
//! 安全底线：
//!   - CA 私钥只落在本机 %LOCALAPPDATA%，不上传、不外发。
//!   - 安装根证书必须由用户在界面上显式点击，绝不静默执行。

use anyhow::{bail, Context, Result};
use rcgen::{
    BasicConstraints, Certificate, CertificateParams, DistinguishedName, DnType, IsCa, KeyPair,
    KeyUsagePurpose,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};

const CA_CN: &str = "SteamPilot Local CA";

fn ca_dir() -> Result<PathBuf> {
    let base = std::env::var("LOCALAPPDATA")
        .or_else(|_| std::env::var("APPDATA"))
        .unwrap_or_else(|_| ".".to_string());
    let dir = PathBuf::from(base).join("SteamPilot").join("ca");
    fs::create_dir_all(&dir).context("create CA directory")?;
    Ok(dir)
}

pub struct CaStore {
    ca_cert: Certificate,
    ca_key: KeyPair,
    leaves: Mutex<HashMap<String, Arc<CertifiedKey>>>,
}

impl std::fmt::Debug for CaStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CaStore")
            .field("leaves", &self.leaves.lock().map(|m| m.len()).unwrap_or(0))
            .finish()
    }
}

impl CaStore {
    /// 加载已有 CA，没有则新建并落盘。
    pub fn load_or_create() -> Result<Self> {
        let dir = ca_dir()?;
        let cert_path = dir.join("ca.crt");
        let key_path = dir.join("ca.key");

        // CA 私钥是根本：只要私钥在，就能重建出同公钥的 CA 证书（序列号不同，本地信任无影响）。
        // 第一次运行时生成并落盘，之后每次从私钥重建，避免 rcgen 无法从 PEM 反序列化 CA 对象的限制。
        let key_pem = if key_path.exists() {
            fs::read_to_string(&key_path).context("read CA key")?
        } else {
            let key = KeyPair::generate().context("generate CA key")?;
            let pem = key.serialize_pem();
            fs::write(&key_path, &pem).context("write CA key")?;
            pem
        };
        let key = KeyPair::from_pem(&key_pem).context("parse CA key")?;

        let mut params = CertificateParams::new(Vec::<String>::new())?;
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, CA_CN.to_string());
        params
            .distinguished_name
            .push(DnType::OrganizationName, "SteamPilot".to_string());
        let cert = params.self_signed(&key).context("build CA cert")?;
        // 每次启动刷新磁盘上的 CA 证书，保证与内存一致
        fs::write(&cert_path, cert.pem()).context("write CA cert")?;

        Ok(Self {
            ca_cert: cert,
            ca_key: key,
            leaves: Mutex::new(HashMap::new()),
        })
    }

    pub fn cert_pem(&self) -> String {
        self.ca_cert.pem()
    }

    pub fn cert_path(&self) -> Result<PathBuf> {
        Ok(ca_dir()?.join("ca.crt"))
    }

    /// 给指定域名签发叶子证书（带缓存）。SAN 与域名完全一致。
    pub fn sign(&self, name: &str) -> Result<Arc<CertifiedKey>> {
        if let Some(k) = self.leaves.lock().unwrap().get(name) {
            return Ok(k.clone());
        }

        let mut params = CertificateParams::new(vec![name.to_string()])
            .with_context(|| format!("leaf params for {}", name))?;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
        let key = KeyPair::generate().context("generate leaf key")?;
        let cert = params
            .signed_by(&key, &self.ca_cert, &self.ca_key)
            .context("sign leaf")?;

        let cert_der = CertificateDer::from(cert.der().as_ref().to_vec());
        let pkcs8 = PrivatePkcs8KeyDer::from(key.serialize_der());
        let private = PrivateKeyDer::Pkcs8(pkcs8);
        let signing_key = rustls::crypto::ring::sign::any_supported_type(&private)
            .context("build ring signing key")?;
        let ck = Arc::new(CertifiedKey {
            cert: vec![cert_der],
            key: signing_key,
            ocsp: None,
        });
        self.leaves
            .lock()
            .unwrap()
            .insert(name.to_string(), ck.clone());
        Ok(ck)
    }

    /// 安装到当前用户的受信任根证书存储。需要用户显式确认后调用。
    pub fn install(&self) -> Result<String> {
        let path = self.cert_path()?;
        let output = Command::new("certutil")
            .args(["-user", "-addstore", "Root"])
            .arg(&path)
            .output()
            .context("run certutil addstore")?;
        let text = String::from_utf8_lossy(&output.stdout).to_string();
        if !output.status.success() {
            let err = String::from_utf8_lossy(&output.stderr).to_string();
            bail!("certutil 安装失败: {} {}", text, err);
        }
        Ok(if text.trim().is_empty() {
            "根证书已安装".to_string()
        } else {
            text.lines().take(3).collect::<Vec<_>>().join(" ")
        })
    }

    /// 从用户证书存储卸载。
    pub fn uninstall(&self) -> Result<String> {
        let output = Command::new("certutil")
            .args(["-user", "-delstore", "Root", CA_CN])
            .output()
            .context("run certutil delstore")?;
        let text = String::from_utf8_lossy(&output.stdout).to_string();
        if !output.status.success() {
            bail!("certutil 卸载失败: {}", text);
        }
        Ok("根证书已卸载".to_string())
    }
}

/// rustls 下游握手时按 ClientHello 的 SNI 动态选证书。
impl ResolvesServerCert for CaStore {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let name = hello.server_name()?;
        self.sign(name).ok()
    }
}
