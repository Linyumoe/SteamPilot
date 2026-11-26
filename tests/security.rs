//! 安全底线回归测试：
//!   1) TLS 验签按 SNI 严格绑定，错误域名必须失败（即"关验签/改错 SNI 必须失败"）；
//!   2) 没有把签发者放进信任根时，即使域名对也必须失败（证明从跳过验证）；
//!   3) 分流决策：GitHub 主站/raw 分片正确，非白名单直连。
//! 全部本地完成，不依赖网络与管理员权限，可进 CI。

use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose,
};
use rustls::client::danger::ServerCertVerifier;
use rustls::client::WebPkiServerVerifier;
use rustls::pki_types::{CertificateDer, DnsName, ServerName, UnixTime};
use std::sync::Arc;
use steampilot_lib::{core, AppState};

/// 生成一个自签 CA 和由它签发的叶子证书（SAN=good.example.com）。返回 (leaf_der, ca_der)。
fn good_cert() -> (CertificateDer<'static>, CertificateDer<'static>) {
    let ca_key = KeyPair::generate().expect("gen ca key");
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).expect("ca params");
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    ca_params.distinguished_name = DistinguishedName::new();
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "SteamPilot Test CA");
    let ca = ca_params.self_signed(&ca_key).expect("self sign ca");

    let leaf_key = KeyPair::generate().expect("gen leaf key");
    let mut leaf_params =
        CertificateParams::new(vec!["good.example.com".to_string()]).expect("leaf params");
    leaf_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    leaf_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let leaf = leaf_params
        .signed_by(&leaf_key, &ca, &ca_key)
        .expect("sign leaf");

    (
        CertificateDer::from(leaf.der().to_vec()),
        CertificateDer::from(ca.der().to_vec()),
    )
}

fn verifier_with(ca: Option<CertificateDer<'static>>) -> Arc<dyn ServerCertVerifier> {
    let mut roots = rustls::RootCertStore::empty();
    if let Some(ca) = ca {
        roots.add(ca).expect("add test CA to roots");
    } else {
        // 用公网根：不含我们的测试 CA，用来验证"没信任就不放行"。
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    }
    WebPkiServerVerifier::builder(Arc::new(roots))
        .build()
        .expect("build verifier")
}

fn verify(v: &dyn ServerCertVerifier, leaf: &CertificateDer<'_>, name: &str) -> bool {
    let dns = DnsName::try_from(name.to_string()).expect("dns name");
    let server_name = ServerName::DnsName(dns);
    v.verify_server_cert(leaf, &[], &server_name, &[], UnixTime::now())
        .is_ok()
}

#[test]
fn sni_bound_to_certificate_wrong_name_rejected() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let (leaf, ca) = good_cert();
    let v = verifier_with(Some(ca));
    assert!(
        verify(&*v, &leaf, "good.example.com"),
        "正确 SNI 应验签通过"
    );
    assert!(
        !verify(&*v, &leaf, "evil.example.com"),
        "错误 SNI 必须验签失败"
    );
    assert!(
        !verify(&*v, &leaf, "good.example.org"),
        "后缀不同也必须失败"
    );
}

#[test]
fn untrusted_issuer_rejected_even_with_right_name() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let (leaf, _ca) = good_cert();
    let v = verifier_with(None); // 公网根，不信任测试 CA
    assert!(
        !verify(&*v, &leaf, "good.example.com"),
        "签发者不被信任时，即使域名正确也必须失败（证明不跳过验签）"
    );
}

#[test]
fn routing_shards_github_and_passes_through_others() {
    let sites = AppState::default().sites;
    match core::decide("github.com", &sites) {
        core::Decision::Accelerate { provider, .. } => {
            assert_eq!(provider, "github-ms", "GitHub 主站应走微软自有 IP 组")
        }
        _ => panic!("github.com 应命中加速"),
    }
    match core::decide("raw.githubusercontent.com", &sites) {
        core::Decision::Accelerate { provider, .. } => {
            assert_eq!(provider, "fastly", "raw 应走 Fastly 别名组")
        }
        _ => panic!("raw.githubusercontent.com 应命中加速"),
    }
    match core::decide("www.baidu.com", &sites) {
        core::Decision::Direct => {}
        _ => panic!("非白名单域名必须直连，不做 MITM"),
    }
}
