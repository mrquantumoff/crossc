# The driver in this bundle is test-signed

`driver\crossc_bth.sys` and `driver\crossc_bth.cat` are signed with a self-signed test certificate
(`CN=WDRLocalTestCert`, shipped as `driver\WDRLocalTestCert.cer`). They are **not** signed by Microsoft and
have not been through attestation signing or WHQL. Windows does not load this driver on a normally configured
PC.

What you have to do, and what it costs you:

* **Test signing mode must be on.** `bcdedit /set testsigning on` from an elevated prompt, then reboot. Windows
  refuses that setting while Secure Boot is on, so Secure Boot has to be turned off in the UEFI setup first
  (suspend BitLocker before you do, or have the recovery key ready). The desktop then shows a "Test Mode"
  watermark, and the kernel loads drivers signed with any code-signing certificate, not only Microsoft's.
* **The certificate must be trusted on this PC**, in the LocalMachine `Root` and `TrustedPublisher` stores.
  `crossc-installer` adds it during Install and takes it out again with "Remove certificate". That is a
  machine-wide trust decision. Check the fingerprints in `BUILD-INFO.txt` against the certificate you are
  about to trust.
* **Memory integrity (HVCI)** may have to be off if the driver fails to load with Code 52 or `0xC0000428`.

About the certificate:

* It is a **throwaway certificate, created for this one build**. The release workflow generates it on the
  GitHub Actions runner while building the driver; the private key is never exported, is deleted from the
  runner's certificate store at the end of the job, and the runner itself is discarded after the job.
* Nobody, the maintainers included, can sign anything else with it afterwards. There is no long-lived signing
  key behind crossc releases, and the signature says nothing about who built the file: it exists only because
  Windows will not install an unsigned driver package even in test mode.
* **Every release is signed with a different certificate.** After updating, the new certificate has to be
  trusted and the old one can be removed.

Use this on a machine you own and can afford to bugcheck. `SHA256SUMS.txt` lists the checksum of every file in
this bundle; the release page carries the checksums of the zip files.
