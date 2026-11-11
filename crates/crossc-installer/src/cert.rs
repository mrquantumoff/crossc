//! Certificate store helpers (CryptoAPI). Every helper takes a store handle, so the add/find/remove logic is tested
//! against an in-memory store and only the callers open LocalMachine stores.

use windows::Win32::Security::Cryptography::{
    CERT_CONTEXT, CERT_FIND_SHA1_HASH, CERT_OPEN_STORE_FLAGS, CERT_QUERY_ENCODING_TYPE,
    CERT_SHA1_HASH_PROP_ID, CERT_STORE_ADD_USE_EXISTING, CERT_STORE_OPEN_EXISTING_FLAG,
    CERT_STORE_PROV_SYSTEM_W, CERT_STORE_READONLY_FLAG, CERT_SYSTEM_STORE_LOCAL_MACHINE,
    CERT_X500_NAME_STR, CRYPT_INTEGER_BLOB, CertAddEncodedCertificateToStore, CertCloseStore,
    CertCreateCertificateContext, CertDeleteCertificateFromStore, CertDuplicateCertificateContext,
    CertEnumCertificatesInStore, CertFindCertificateInStore, CertFreeCertificateContext,
    CertGetCertificateContextProperty, CertNameToStrW, HCERTSTORE, PKCS_7_ASN_ENCODING,
    X509_ASN_ENCODING,
};
use windows::core::HSTRING;

use crate::parse::hex_upper;

/// Subject of the WDK's local test-signing certificate (what the script's Remove-Cert matches).
pub const TEST_CERT_SUBJECT: &str = "CN=WDRLocalTestCert";
/// The machine stores the driver needs the certificate in.
pub const TRUST_STORES: [&str; 2] = ["Root", "TrustedPublisher"];

const ENCODING: CERT_QUERY_ENCODING_TYPE =
    CERT_QUERY_ENCODING_TYPE(X509_ASN_ENCODING.0 | PKCS_7_ASN_ENCODING.0);

/// An open certificate store, closed on drop.
pub struct Store(HCERTSTORE);

impl Drop for Store {
    fn drop(&mut self) {
        unsafe {
            let _ = CertCloseStore(Some(self.0), 0);
        }
    }
}

impl Store {
    /// `LocalMachine\<name>`; read-only opens never need elevation.
    pub fn local_machine(name: &str, read_only: bool) -> windows::core::Result<Store> {
        let mut flags = CERT_SYSTEM_STORE_LOCAL_MACHINE | CERT_STORE_OPEN_EXISTING_FLAG.0;
        if read_only {
            flags |= CERT_STORE_READONLY_FLAG.0;
        }
        let name = HSTRING::from(name);
        let h = unsafe {
            open_store(
                CERT_STORE_PROV_SYSTEM_W,
                CERT_OPEN_STORE_FLAGS(flags),
                Some(name.as_ptr().cast()),
            )?
        };
        Ok(Store(h))
    }

    /// A fresh in-memory store (tests).
    #[cfg(test)]
    pub fn memory() -> windows::core::Result<Store> {
        use windows::Win32::Security::Cryptography::CERT_STORE_PROV_MEMORY;
        Ok(Store(unsafe {
            open_store(CERT_STORE_PROV_MEMORY, CERT_OPEN_STORE_FLAGS(0), None)?
        }))
    }

    pub fn contains_sha1(&self, sha1: &[u8; 20]) -> bool {
        let blob = CRYPT_INTEGER_BLOB {
            cbData: 20,
            pbData: sha1.as_ptr().cast_mut(),
        };
        unsafe {
            let ctx = CertFindCertificateInStore(
                self.0,
                ENCODING,
                0,
                CERT_FIND_SHA1_HASH,
                Some((&raw const blob).cast()),
                None,
            );
            if ctx.is_null() {
                false
            } else {
                let _ = CertFreeCertificateContext(Some(ctx));
                true
            }
        }
    }

    /// Adds a DER certificate; an identical one already present is kept.
    pub fn add_der(&self, der: &[u8]) -> windows::core::Result<()> {
        unsafe {
            CertAddEncodedCertificateToStore(
                Some(self.0),
                X509_ASN_ENCODING,
                der,
                CERT_STORE_ADD_USE_EXISTING,
                None,
            )
        }
    }

    /// Deletes the certificate with this SHA-1 thumbprint, and only if its subject is exactly `subject`: the way
    /// the certificate of an earlier crossc release is untrusted once a newer one has taken its place. A
    /// certificate with another thumbprint is never touched, whatever its subject, and neither is one with this
    /// thumbprint and another subject.
    pub fn remove_exact(&self, sha1: &[u8; 20], subject: &str) -> Removal {
        let blob = CRYPT_INTEGER_BLOB {
            cbData: 20,
            pbData: sha1.as_ptr().cast_mut(),
        };
        // As in `remove_by_subject`: collect duplicates first, delete afterwards.
        let mut doomed: Vec<*const CERT_CONTEXT> = Vec::new();
        unsafe {
            let mut prev: *mut CERT_CONTEXT = std::ptr::null_mut();
            loop {
                let ctx = CertFindCertificateInStore(
                    self.0,
                    ENCODING,
                    0,
                    CERT_FIND_SHA1_HASH,
                    Some((&raw const blob).cast()),
                    (!prev.is_null()).then_some(prev.cast_const()),
                );
                if ctx.is_null() {
                    break;
                }
                if sha1_of(ctx).as_ref() == Some(sha1)
                    && subject_of(ctx).as_deref() == Some(subject)
                {
                    doomed.push(CertDuplicateCertificateContext(Some(ctx)).cast_const());
                }
                prev = ctx;
            }
        }
        delete_all(doomed)
    }

    /// Deletes every certificate whose subject is exactly `subject`. Returns the SHA-1 thumbprints removed and, for
    /// any that could not be deleted, the thumbprint and error; one failure does not stop the remaining deletes.
    pub fn remove_by_subject(&self, subject: &str) -> Removal {
        // Collect duplicates first: CertDeleteCertificateFromStore frees the context it is given, which would break
        // the enumeration if it were the "previous" context.
        let mut doomed: Vec<*const CERT_CONTEXT> = Vec::new();
        unsafe {
            let mut prev: *mut CERT_CONTEXT = std::ptr::null_mut();
            loop {
                let ctx = CertEnumCertificatesInStore(
                    self.0,
                    (!prev.is_null()).then_some(prev.cast_const()),
                );
                if ctx.is_null() {
                    break;
                }
                if subject_of(ctx).as_deref() == Some(subject) {
                    doomed.push(CertDuplicateCertificateContext(Some(ctx)).cast_const());
                }
                prev = ctx;
            }
        }
        delete_all(doomed)
    }
}

/// Deletes the collected certificate contexts from their store.
fn delete_all(doomed: Vec<*const CERT_CONTEXT>) -> Removal {
    let mut out = Removal::default();
    for ctx in doomed {
        let tp = sha1_of(ctx).map(|h| hex_upper(&h)).unwrap_or_default();
        // CertDeleteCertificateFromStore always frees `ctx`, even when the delete fails, so nothing leaks here.
        match unsafe { CertDeleteCertificateFromStore(ctx) } {
            Ok(()) => out.removed.push(tp),
            Err(e) => out.failed.push((tp, e.to_string())),
        }
    }
    out
}

/// Result of [`Store::remove_by_subject`] and [`Store::remove_exact`]: thumbprints removed, and (thumbprint, error) for those that were not.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Removal {
    pub removed: Vec<String>,
    pub failed: Vec<(String, String)>,
}

// CertOpenStore with the generic provider/para parameters spelled out once.
unsafe fn open_store(
    provider: windows::core::PCSTR,
    flags: CERT_OPEN_STORE_FLAGS,
    para: Option<*const core::ffi::c_void>,
) -> windows::core::Result<HCERTSTORE> {
    unsafe {
        windows::Win32::Security::Cryptography::CertOpenStore(
            provider,
            CERT_QUERY_ENCODING_TYPE(0),
            None,
            flags,
            para,
        )
    }
}

fn subject_of(ctx: *const CERT_CONTEXT) -> Option<String> {
    unsafe {
        let name = &raw const (*(*ctx).pCertInfo).Subject;
        let len = CertNameToStrW(X509_ASN_ENCODING, name, CERT_X500_NAME_STR, None);
        if len <= 1 {
            return None;
        }
        let mut buf = vec![0u16; len as usize];
        let n = CertNameToStrW(X509_ASN_ENCODING, name, CERT_X500_NAME_STR, Some(&mut buf));
        Some(String::from_utf16_lossy(
            &buf[..n.saturating_sub(1) as usize],
        ))
    }
}

fn sha1_of(ctx: *const CERT_CONTEXT) -> Option<[u8; 20]> {
    let mut hash = [0u8; 20];
    let mut len = 20u32;
    unsafe {
        CertGetCertificateContextProperty(
            ctx,
            CERT_SHA1_HASH_PROP_ID,
            Some(hash.as_mut_ptr().cast()),
            &mut len,
        )
        .ok()?
    };
    (len == 20).then_some(hash)
}

/// SHA-1 thumbprint of a DER certificate (what PowerShell shows as `Thumbprint`).
pub fn thumbprint(der: &[u8]) -> windows::core::Result<[u8; 20]> {
    unsafe {
        let ctx = CertCreateCertificateContext(X509_ASN_ENCODING, der);
        if ctx.is_null() {
            return Err(windows::core::Error::from_thread());
        }
        let r = sha1_of(ctx);
        let _ = CertFreeCertificateContext(Some(ctx));
        r.ok_or_else(windows::core::Error::from_thread)
    }
}

/// Subject (`CN=WDRLocalTestCert`) and SHA-1 thumbprint of a DER certificate: what the user is shown before a
/// downloaded release's certificate is trusted.
pub fn describe(der: &[u8]) -> windows::core::Result<(String, [u8; 20])> {
    unsafe {
        let ctx = CertCreateCertificateContext(X509_ASN_ENCODING, der);
        if ctx.is_null() {
            return Err(windows::core::Error::from_thread());
        }
        let r = sha1_of(ctx).map(|tp| (subject_of(ctx).unwrap_or_default(), tp));
        let _ = CertFreeCertificateContext(Some(ctx));
        r.ok_or_else(windows::core::Error::from_thread)
    }
}

/// `0123...EF` (40 hex digits, any case) as the 20 bytes of a SHA-1 thumbprint.
pub fn parse_thumbprint(hex: &str) -> Option<[u8; 20]> {
    if hex.len() != 40 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 20];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// Self-signed throwaway certificates made with openssl for these tests (their private keys were deleted
    /// right away): two different ones with the subject of the WDK test certificate, one with another subject.
    pub const WDR_A: &[u8] = include_bytes!("../testdata/wdr-a.cer");
    pub const WDR_A_THUMBPRINT: &str = "00E56600170648F8D7B71EACA4392DBFCFC615FD";
    pub const WDR_B: &[u8] = include_bytes!("../testdata/wdr-b.cer");
    pub const WDR_B_THUMBPRINT: &str = "BF82EE9838ED6EE917234D4EF9319BC4EF8C39FF";
    pub const OTHER: &[u8] = include_bytes!("../testdata/other.cer");
    pub const OTHER_THUMBPRINT: &str = "48427A1D1514F7FF16E30738987DC01E6944F80B";

    fn tp_bytes(hex: &str) -> [u8; 20] {
        parse_thumbprint(hex).unwrap()
    }

    #[test]
    fn certificates_are_described() {
        for (der, subject, thumbprint_hex) in [
            (WDR_A, TEST_CERT_SUBJECT, WDR_A_THUMBPRINT),
            (WDR_B, TEST_CERT_SUBJECT, WDR_B_THUMBPRINT),
            (OTHER, "CN=SomebodyElse", OTHER_THUMBPRINT),
        ] {
            let (found_subject, tp) = describe(der).unwrap();
            assert_eq!(found_subject, subject);
            assert_eq!(hex_upper(&tp), thumbprint_hex);
            assert_eq!(thumbprint(der).unwrap(), tp);
        }
        assert!(describe(b"not a certificate").is_err());
        assert!(describe(&WDR_A[..100]).is_err());
    }

    #[test]
    fn thumbprints_are_parsed() {
        assert_eq!(hex_upper(&tp_bytes(WDR_A_THUMBPRINT)), WDR_A_THUMBPRINT);
        assert_eq!(
            tp_bytes(&WDR_A_THUMBPRINT.to_lowercase()),
            tp_bytes(WDR_A_THUMBPRINT)
        );
        let too_long = format!("{WDR_A_THUMBPRINT}00");
        let not_hex = "G".repeat(40);
        let not_ascii = "\u{e9}".repeat(20);
        let signed = format!("+{}", &WDR_A_THUMBPRINT[1..]);
        for bad in [
            "",
            "00E5",
            &WDR_A_THUMBPRINT[1..],
            &too_long,
            &not_hex,
            &not_ascii,
            &signed,
        ] {
            assert_eq!(parse_thumbprint(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_previous_certificate_is_removed_by_thumbprint_and_subject_only() {
        let store = Store::memory().unwrap();
        for der in [WDR_A, WDR_B, OTHER] {
            store.add_der(der).unwrap();
        }
        let has = |hex: &str| store.contains_sha1(&tp_bytes(hex));
        assert!(has(WDR_A_THUMBPRINT) && has(WDR_B_THUMBPRINT) && has(OTHER_THUMBPRINT));

        // The right thumbprint with the wrong subject: nothing happens. This is the case of a record that names
        // some certificate that is not crossc's.
        assert_eq!(
            store.remove_exact(&tp_bytes(OTHER_THUMBPRINT), TEST_CERT_SUBJECT),
            Removal::default()
        );
        assert_eq!(
            store.remove_exact(&tp_bytes(WDR_A_THUMBPRINT), "CN=SomebodyElse"),
            Removal::default()
        );
        // A thumbprint no certificate has.
        assert_eq!(
            store.remove_exact(&[0x11; 20], TEST_CERT_SUBJECT),
            Removal::default()
        );
        assert!(has(WDR_A_THUMBPRINT) && has(WDR_B_THUMBPRINT) && has(OTHER_THUMBPRINT));

        // Thumbprint and subject both match: that one certificate goes, the other WDRLocalTestCert (the release
        // that replaced it) and the unrelated certificate stay.
        assert_eq!(
            store.remove_exact(&tp_bytes(WDR_A_THUMBPRINT), TEST_CERT_SUBJECT),
            Removal {
                removed: vec![WDR_A_THUMBPRINT.to_string()],
                failed: vec![]
            }
        );
        assert!(!has(WDR_A_THUMBPRINT));
        assert!(has(WDR_B_THUMBPRINT) && has(OTHER_THUMBPRINT));
        // Again: nothing left to remove.
        assert_eq!(
            store.remove_exact(&tp_bytes(WDR_A_THUMBPRINT), TEST_CERT_SUBJECT),
            Removal::default()
        );
    }

    #[test]
    fn remove_by_subject_takes_every_test_certificate_and_nothing_else() {
        let store = Store::memory().unwrap();
        for der in [WDR_A, WDR_B, OTHER] {
            store.add_der(der).unwrap();
        }
        let mut removal = store.remove_by_subject(TEST_CERT_SUBJECT);
        removal.removed.sort();
        assert_eq!(
            removal,
            Removal {
                removed: vec![WDR_A_THUMBPRINT.to_string(), WDR_B_THUMBPRINT.to_string()],
                failed: vec![]
            }
        );
        assert!(store.contains_sha1(&tp_bytes(OTHER_THUMBPRINT)));
    }

    /// The real test certificate from a built package; the test is skipped when nothing is built.
    fn package_cert() -> Option<Vec<u8>> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../drivers/crossc-bth/target");
        ["debug", "release"].iter().find_map(|f| {
            std::fs::read(
                root.join(f)
                    .join("crossc_bth_package")
                    .join("WDRLocalTestCert.cer"),
            )
            .ok()
        })
    }

    #[test]
    fn memory_store_round_trip() {
        let Some(der) = package_cert() else {
            eprintln!("no built package: skipping certificate round trip");
            return;
        };
        let tp = thumbprint(&der).unwrap();
        let store = Store::memory().unwrap();
        assert!(!store.contains_sha1(&tp));
        store.add_der(&der).unwrap();
        store.add_der(&der).unwrap(); // idempotent
        assert!(store.contains_sha1(&tp));
        assert_eq!(
            store.remove_by_subject("CN=SomethingElse"),
            Removal::default()
        );
        assert_eq!(
            store.remove_by_subject(TEST_CERT_SUBJECT),
            Removal {
                removed: vec![hex_upper(&tp)],
                failed: vec![]
            }
        );
        assert!(!store.contains_sha1(&tp));
        assert_eq!(
            store.remove_by_subject(TEST_CERT_SUBJECT),
            Removal::default()
        );
    }

    #[test]
    fn garbage_is_not_a_certificate() {
        assert!(thumbprint(b"not a certificate").is_err());
    }
}
