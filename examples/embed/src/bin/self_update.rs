//! A program that updates its own executable with `lode_core::self_update`.
//!
//! The demo is offline and plays both sides:
//!
//!   * the publisher — builds release 2.0.0 of `myapp`, signs the asset and
//!     the manifest, and serves both on loopback (in real life: CI running
//!     `lode-cli sign` / `lode-cli manifest-sign`, and a web server or
//!     registry);
//!   * the installed program — 1.0.0 of `myapp`, which checks the source,
//!     downloads and verifies the release, takes a companion file out of it
//!     and replaces its own executable.
//!
//! ```bash
//! cargo run --manifest-path examples/embed/Cargo.toml --bin self_update
//! ```

#[cfg(not(unix))]
fn main() {
    eprintln!("this demo stands in a shell script for the program and runs on Unix only");
}

#[cfg(unix)]
fn main() -> std::process::ExitCode {
    match demo::run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("demo: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(unix)]
mod demo {
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;
    use std::process::Command;

    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
    use ed25519_dalek::{Signer as _, SigningKey};
    use lode_core::self_update::{Outcome, SelfUpdate, SelfUpdateError, Source};
    use lode_core::verify::{Artifact, artifact_message, key_id, sha256_hex};
    use lode_core::{InitOptions, RequireManifest};
    use serde_json::json;

    const APP: &str = "myapp";
    const ASSET: &str = "myapp-linux-x64.tar.gz";

    type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

    pub fn run() -> Result<()> {
        // Process-wide effects are opt-in. A real program adds
        // `.crypto_provider(true)` here: the first HTTPS fetch needs it.
        InitOptions::new().log_level("warn").install();

        let dir =
            std::env::temp_dir().join(format!("lode-demo-self-update-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("bin"))?;
        let result = walk_through(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        result
    }

    fn walk_through(dir: &Path) -> Result<()> {
        // The installed program. A real one passes `self_update::current_exe()`,
        // resolved once at startup.
        let exe = dir.join("bin").join(APP);
        write_program(&exe, "1.0.0")?;
        println!("installed:  {}", version_of(&exe)?);

        let release = publish("2.0.0")?;

        // --- from here on: what the installed program does ---------------------
        let update = SelfUpdate::new(APP, Source::Manifest(release.manifest_url.clone()), "1.0.0")
            .asset(ASSET)
            // Compiled into a real program, never read from its configuration.
            .trusted_keys([release.trusted_key.clone()])
            // Refuse a manifest without a signed, unexpired expiry: a source
            // cannot hold the program on an old version by replaying one.
            .require_manifest(RequireManifest::Fresh)
            .exe(&exe);

        // 1. Look, without downloading. Notes and time are unsigned: display only.
        let Some(available) = update.check()? else {
            return Err("the source offers nothing newer".into());
        };
        println!(
            "available:  {} (published {}): {}",
            available.version,
            available.entry.published_at.as_deref().unwrap_or("?"),
            available.entry.notes.as_deref().unwrap_or(""),
        );

        // 2. Download, verify and unpack. Nothing is installed yet.
        let Some(fetched) = update.fetch()? else {
            return Err("the source offers nothing newer".into());
        };
        // The whole verified asset is under `fetched.dir()`: take what the
        // program ships besides its executable.
        let plugin = exe.with_file_name("plugin.dat");
        std::fs::copy(fetched.dir().join("plugin.dat"), &plugin)?;
        println!("companion:  {}", std::fs::read_to_string(&plugin)?.trim());

        // 3. Prove the new binary runs and reports its version, then replace
        //    the executable. The running process stays the old one.
        match update.install(&fetched)? {
            Outcome::Installed { version, exe } => {
                println!("installed:  {version} at {}", exe.display());
            }
            Outcome::UpToDate => {}
        }
        println!("now runs:   {}", version_of(&exe)?);

        // The program is 2.0.0 now and asks again.
        let again = SelfUpdate::new(APP, Source::Manifest(release.manifest_url.clone()), "2.0.0")
            .asset(ASSET)
            .trusted_keys([release.trusted_key])
            .exe(&exe);
        println!("check:      {:?}", again.check()?.map(|a| a.version));

        // Errors are an enum. A program maps the variants to its own messages
        // and asks whether anything was changed.
        let stranger = SelfUpdate::new(APP, Source::Manifest(release.manifest_url), "1.0.0")
            .asset(ASSET)
            .trusted_keys([publisher_key().1])
            .exe(&exe);
        match stranger.apply() {
            Err(error @ SelfUpdateError::Verify(_)) => println!(
                "wrong key:  refused ({error}); installation untouched: {}",
                error.is_untouched()
            ),
            other => return Err(format!("expected a verification failure, got {other:?}").into()),
        }
        Ok(())
    }

    /// What the publisher hands out: where the manifest is, and the key entry
    /// that programs compile in.
    struct Release {
        manifest_url: String,
        trusted_key: String,
    }

    /// A fresh ed25519 key and its `<key_id>:<public key>` entry, as
    /// `lode-cli keygen` prints it.
    fn publisher_key() -> (SigningKey, String) {
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).expect("randomness");
        let key = SigningKey::from_bytes(&seed);
        let public = key.verifying_key().to_bytes();
        let entry = format!("{}:{}", key_id(&public), B64.encode(public));
        (key, entry)
    }

    /// Build, sign and serve release `version`. Everything here is the
    /// publisher's side; `lode-cli sign --app myapp` and
    /// `lode-cli manifest-sign --expires-in` produce the same signatures.
    fn publish(version: &str) -> Result<Release> {
        let (key, trusted_key) = publisher_key();
        let id = trusted_key.split(':').next().unwrap_or_default().to_owned();

        let asset = archive(version)?;
        let sha256 = sha256_hex(&asset);
        // The asset signature names the app, the file, the version and the digest.
        let artifact = Artifact {
            app: APP,
            name: ASSET,
            version,
            path: "",
            run: None,
            exec: None,
        };
        let sig = B64.encode(key.sign(&artifact_message(&artifact, &sha256)).to_bytes());

        let listener = TcpListener::bind("127.0.0.1:0")?;
        let base = format!("http://127.0.0.1:{}", listener.local_addr()?.port());
        let mut document = json!({
            "schema": "lode/v1",
            "name": APP,
            "channels": { "stable": { "latest": version } },
            "versions": { version: {
                "notes": "Faster startup.",
                "published_at": "2026-10-12T10:00:00Z",
                "assets": [{
                    "name": ASSET,
                    "url": format!("{base}/{ASSET}"),
                    "sha256": sha256,
                    "sig": sig,
                }],
            } },
        });

        // The manifest signature covers the channels and every asset digest;
        // the expiry signature makes an old copy of it worthless after a day.
        let mut manifest = lode_core::manifest::parse(document.to_string().as_bytes())?;
        manifest.key_id = Some(id.clone());
        let expires = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs()
            + 24 * 60 * 60;
        manifest.expires = Some(expires);
        let expiry_message = manifest
            .expiry_signing_message()
            .ok_or("no expiry to sign")?;
        document["key_id"] = json!(id);
        document["sig"] = json!(B64.encode(key.sign(&manifest.signing_message()).to_bytes()));
        document["expires"] = json!(expires);
        document["expires_sig"] = json!(B64.encode(key.sign(&expiry_message).to_bytes()));

        serve(listener, document.to_string().into_bytes(), asset);
        Ok(Release {
            manifest_url: format!("{base}/manifest.json"),
            trusted_key,
        })
    }

    /// The release asset: the program and a file that ships with it.
    fn archive(version: &str) -> Result<Vec<u8>> {
        let mut builder = tar::Builder::new(Vec::new());
        for (name, body, mode) in [
            (APP, program(version), 0o755),
            ("plugin.dat", format!("plugin for {version}\n"), 0o644),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(mode);
            header.set_cksum();
            builder.append_data(&mut header, name, body.as_bytes())?;
        }
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&builder.into_inner()?)?;
        Ok(gz.finish()?)
    }

    /// A stand-in for the program: all it does is report its version.
    fn program(version: &str) -> String {
        format!("#!/bin/sh\necho \"{APP} {version}\"\n")
    }

    fn write_program(path: &Path, version: &str) -> Result<()> {
        std::fs::write(path, program(version))?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
        Ok(())
    }

    fn version_of(exe: &Path) -> Result<String> {
        let output = Command::new(exe).arg("--version").output()?;
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    /// Answer `GET /manifest.json` and `GET /<asset>` on loopback, where lode
    /// allows plain http.
    fn serve(listener: TcpListener, manifest: Vec<u8>, asset: Vec<u8>) {
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap_or(0) == 1 {
                    head.push(byte[0]);
                }
                let request = String::from_utf8_lossy(&head);
                let path = request.split_whitespace().nth(1).unwrap_or_default();
                let body: &[u8] = if path == "/manifest.json" {
                    &manifest
                } else {
                    &asset
                };
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .as_bytes(),
                );
                let _ = stream.write_all(body);
            }
        });
    }
}
