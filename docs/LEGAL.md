# Legal

## License

Nyx Backup Recovery is licensed under the **Apache License, Version 2.0** - see
[LICENSE](LICENSE).  You may use, copy, modify, and redistribute it,
including in proprietary products, provided the copyright notice and
license text are preserved.

The main **Nyx Backup** application is a separate, proprietary product
and is NOT covered by this license.  Only this recovery tool is open
source.

## Trademarks

"Nyx Backup" and the Nyx Backup logo are trademarks of
Nyx Software, LLC.  The Apache-2.0 license grants rights to the **source code** only;
it does NOT grant any right to use these names or marks.  In particular:

- You may fork and redistribute this code, but a redistributed or
  modified build must not be presented as the official Nyx Backup
  Recovery tool, and must not use the Nyx Backup name or logo in a way
  that implies endorsement by or affiliation with Nyx Software, LLC.
- Please rename forks that you distribute publicly so users are not
  misled about the source.

## No warranty

As stated in the Apache-2.0 license, the software is provided "AS IS", without
warranty of any kind.  This is disaster-recovery software: while it is
designed and tested to restore data faithfully, you are responsible for
verifying restored data.  Nyx Software, LLC is not liable for any data loss,
corruption, or other damages arising from its use.

## Third-party software

This tool is built on open-source Rust crates and the Tauri / WebView
runtime, each under its own license (predominantly MIT and Apache-2.0).
A complete dependency manifest is in `Cargo.lock`.  To generate an
attribution report of all bundled third-party licenses, run:

```
cargo install cargo-about
cargo about generate about.hbs > THIRD_PARTY_LICENSES.html
```

The bundled WebView2 runtime loader (`WebView2Loader.dll`, Windows) is
redistributed under the Microsoft WebView2 distribution terms.

## Cryptography / export notice

This software contains and uses encryption (AES-256-GCM, SHA-256,
HKDF, Argon2id, Ed25519) for the sole purpose of decrypting the user's
own backups.  It uses standard, widely-available cryptographic
algorithms and does not implement novel cryptography.

Because the source is published openly, it falls under the streamlined
treatment that US export regulations (EAR) provide for publicly
available open-source encryption software.  Distributors outside the US,
or anyone redistributing modified binaries, are responsible for their own
compliance with applicable export-control and cryptography-import laws.

> This section is informational, not legal advice.  Confirm
> classification and any notification requirements with qualified
> counsel before commercial distribution.

## Contact

Nyx Software, LLC - https://nyxbackup.com - legal@nyxbackup.com
