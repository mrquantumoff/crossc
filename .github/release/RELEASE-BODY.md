## Test-signed driver: read before installing

The kernel driver in `crossc-@@VERSION@@-x64.zip` is **test-signed**. It is not signed by Microsoft.

* Windows loads it only in **test signing mode** (`bcdedit /set testsigning on`, which requires Secure Boot to be
  off), and only after the certificate in the zip (`driver\WDRLocalTestCert.cer`) has been trusted on the PC.
  `crossc-installer` does the certificate part and can undo it.
* The certificate is a **throwaway, made for this one build** on the GitHub Actions runner. Its private key was
  never exported, was deleted at the end of the build job and is gone with the runner. Nobody can sign anything
  else with it, and the next release is signed with a different one.
* `TEST-SIGNING.md` in the zip has the details; `BUILD-INFO.txt` has the certificate fingerprints and the commit.

## Updating from 0.2.0

The installer of 0.2.0 does not know the tray app. Its **Download and install** updates the driver, the service
and the CLI, and leaves `crossc-tray.exe` in the unpacked release (`%ProgramFiles%\crossc\releases\<tag>`)
without installing it. To get the tray icon, run the new `crossc-installer.exe` (from that folder, or from the
zip below) and press **Install** once. Later updates install the tray with everything else.

## Files

| File | Contents |
|---|---|
| `crossc-@@VERSION@@-x64.zip` | crossc-installer, crossc-service, crossc-tray (the tray app), crossc-cli, the driver package (`driver\`), license notices |
| `crossc-@@VERSION@@-symbols.zip` | `.pdb` files for the driver and the programs |
| `SHA256SUMS.txt` | SHA-256 of the two zip files (`sha256sum -c SHA256SUMS.txt`) |
