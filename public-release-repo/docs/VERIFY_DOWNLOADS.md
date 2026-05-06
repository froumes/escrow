# Verifying TWM Downloads

Each release includes a `SHA256SUMS.txt` file with hashes for every binary asset.

## Windows

Run:

```powershell
certutil -hashfile .\twm-windows-x86_64.exe SHA256
certutil -hashfile .\TWM-loader-windows-x86_64.exe SHA256
```

## Linux

Run:

```bash
sha256sum twm-linux-x86_64
sha256sum TWM-loader-linux-x86_64
```

Compare the printed hashes with the matching lines in `SHA256SUMS.txt`.
