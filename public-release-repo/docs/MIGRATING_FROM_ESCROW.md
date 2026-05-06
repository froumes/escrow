# Migrating From `froumes/escrow`

If you previously downloaded TWM or `TWM-loader` from `froumes/escrow`, you should switch to this repository for future updates.

## What changed

Releases now live in this repository instead of the old one.

Old loader binaries do not automatically change which GitHub repo they check. They keep looking at the repo they were built with.

## What happens if you keep the old loader

If `froumes/escrow` becomes private:

- the old loader will fail its update check
- it will usually continue by launching your existing local `twm` binary
- you will stop receiving new releases automatically

## One-time fix

Download the latest loader from this repository and replace your old loader file.

### Windows

1. Download `TWM-loader-windows-x86_64.exe` from the latest release.
2. Place it in the same folder where you currently keep `twm.exe`.
3. Use the new loader from then on.

### Linux

1. Download `TWM-loader-linux-x86_64` from the latest release.
2. Put it next to your existing `twm` binary.
3. Make sure it is executable:

```bash
chmod +x TWM-loader-linux-x86_64
```

4. Run the new loader from then on.

## After migration

Once you are using the loader from this repository:

- future update checks will use this repository
- the loader will update the main `twm` binary normally
- you should only need another manual loader replacement if a future release changes loader behavior itself
