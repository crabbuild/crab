use super::*;
use std::cell::Cell;

struct Fixture {
    directory: tempfile::TempDir,
    objects: PathBuf,
    oid: String,
    bytes: Vec<u8>,
}

fn git(directory: &Path, args: &[&str], input: &[u8]) -> io::Result<Vec<u8>> {
    let mut stdin = tempfile::tempfile()?;
    stdin.write_all(input)?;
    stdin.rewind()?;
    let output = std::process::Command::new("git")
        .current_dir(directory)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", directory.join("gitconfig"))
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .stdin(stdin)
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output.stdout)
}

fn fixture(hash: &str, offset: bool) -> io::Result<Fixture> {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    std::fs::write(root.join("gitconfig"), "")?;
    let format = format!("--object-format={hash}");
    git(root, &["init", "--bare", "--template=", &format], b"")?;
    let mut base = vec![0; 1024 * 1024];
    base[0] = b'a';
    let mut target = base.clone();
    target[1024 * 1024 - 1] = b'b';
    let mut oids = Vec::new();
    for body in [&base, &target] {
        let output = git(
            root,
            &["hash-object", "--no-filters", "-w", "--stdin"],
            body,
        )?;
        oids.push(
            String::from_utf8(output)
                .map_err(io::Error::other)?
                .trim()
                .to_owned(),
        );
    }
    let mut args = vec!["pack-objects", "--threads=1", "--window=10", "--depth=10"];
    if offset {
        args.push("--delta-base-offset");
    }
    args.push("objects/pack/pack");
    let input = oids.join("\n") + "\n";
    git(root, &args, input.as_bytes())?;
    git(root, &["prune-packed"], b"")?;
    let objects = root.join("objects");
    for (oid, bytes) in oids.iter().zip([base, target]) {
        assert!(!objects.join(&oid[..2]).join(&oid[2..]).exists());
        let raw = oid_bytes(oid)?;
        let mut location =
            locate(&objects, &raw, &|| false)?.ok_or_else(|| invalid("missing fixture object"))?;
        let entry = header(&mut location, raw.len())?;
        let is_delta = if offset {
            matches!(entry.base, Base::Offset(_))
        } else {
            matches!(entry.base, Base::Reference(_))
        };
        if is_delta {
            return Ok(Fixture {
                directory,
                objects,
                oid: oid.clone(),
                bytes,
            });
        }
    }
    Err(invalid("Git did not create the requested delta format"))
}

#[test]
fn packed_offset_and_reference_deltas_preserve_sha1_and_sha256_bytes() -> io::Result<()> {
    for hash in ["sha1", "sha256"] {
        for offset in [false, true] {
            let fixture = fixture(hash, offset)?;
            let mut output = Vec::new();
            let space = ScratchSpace::new(fixture.directory.path(), 8 * 1024 * 1024_u64);
            let scratch = || Ok(space);
            assert!(copy_packed_blob(
                &fixture.objects,
                &fixture.oid,
                fixture.bytes.len() as u64,
                &mut output,
                scratch,
                &|| false
            )?);
            assert_eq!(output, fixture.bytes, "{hash}, offset={offset}");
        }
    }
    Ok(())
}

#[test]
fn packed_delta_disk_rejection_releases_anonymous_scratch() -> io::Result<()> {
    let fixture = fixture("sha1", false)?;
    let scratch = fixture.directory.path().join("scratch");
    std::fs::create_dir(&scratch)?;
    let space = ScratchSpace::new(&scratch, 1024_u64);
    let mut output = Vec::new();
    let result = copy_packed_blob(
        &fixture.objects,
        &fixture.oid,
        fixture.bytes.len() as u64,
        &mut output,
        || Ok(space.clone()),
        &|| false,
    );
    let error = result
        .err()
        .ok_or_else(|| invalid("expected scratch rejection"))?;
    assert!(
        error
            .get_ref()
            .is_some_and(|source| source.is::<crate::incoming_pack::ScratchError>())
    );
    assert_eq!(std::fs::read_dir(&scratch)?.count(), 0);
    let mut next = space.anonymous_file()?;
    next.write_all(&[0; 1024])?;
    Ok(())
}

#[test]
fn packed_delta_cancellation_after_output_closes_all_scratch() -> io::Result<()> {
    struct StopWriter<'a> {
        stop: &'a Cell<bool>,
        written: usize,
    }
    impl Write for StopWriter<'_> {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.stop.set(true);
            self.written += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let fixture = fixture("sha1", true)?;
    let scratch = fixture.directory.path().join("scratch");
    std::fs::create_dir(&scratch)?;
    let space = ScratchSpace::new(&scratch, 8 * 1024 * 1024_u64);
    let stop = Cell::new(false);
    let mut output = StopWriter {
        stop: &stop,
        written: 0,
    };
    let result = copy_packed_blob(
        &fixture.objects,
        &fixture.oid,
        fixture.bytes.len() as u64,
        &mut output,
        || Ok(space),
        &|| stop.get(),
    );
    let error = result
        .err()
        .ok_or_else(|| invalid("expected cancellation"))?;
    assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    assert!(output.written > 0);
    assert_eq!(std::fs::read_dir(scratch)?.count(), 0);
    Ok(())
}

#[test]
fn missing_packed_identity_does_not_create_scratch() -> io::Result<()> {
    let fixture = fixture("sha1", false)?;
    let scratch = || Err(io::Error::other("scratch must not be requested"));
    assert!(!copy_packed_blob(
        &fixture.objects,
        &"0".repeat(40),
        0,
        &mut Vec::new(),
        scratch,
        &|| false
    )?);
    Ok(())
}

#[test]
fn reference_base_in_its_pack_precedes_a_corrupt_loose_alias() -> io::Result<()> {
    let fixture = fixture("sha1", false)?;
    let raw = oid_bytes(&fixture.oid)?;
    let mut location =
        locate(&fixture.objects, &raw, &|| false)?.ok_or_else(|| invalid("missing delta"))?;
    let entry = header(&mut location, raw.len())?;
    let Base::Reference(base) = entry.base else {
        return Err(invalid("expected reference delta"));
    };
    let oid = base
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let directory = fixture.objects.join(&oid[..2]);
    std::fs::create_dir_all(&directory)?;
    std::fs::write(directory.join(&oid[2..]), b"corrupt external alias")?;
    let native = git(
        fixture.directory.path(),
        &["cat-file", "blob", &fixture.oid],
        b"",
    )?;
    assert_eq!(native, fixture.bytes);
    let mut output = Vec::new();
    let scratch = || {
        Ok(ScratchSpace::new(
            fixture.directory.path(),
            8 * 1024 * 1024_u64,
        ))
    };
    assert!(copy_packed_blob(
        &fixture.objects,
        &fixture.oid,
        fixture.bytes.len() as u64,
        &mut output,
        scratch,
        &|| false
    )?);
    assert_eq!(output, native);
    Ok(())
}

#[test]
fn version_one_indexes_preserve_native_delta_bytes() -> io::Result<()> {
    let fixture = fixture("sha1", true)?;
    let raw = oid_bytes(&fixture.oid)?;
    let location =
        locate(&fixture.objects, &raw, &|| false)?.ok_or_else(|| invalid("missing delta"))?;
    let index = location.path.with_extension("idx");
    let replacement = fixture.directory.path().join("version-one.idx");
    git(
        fixture.directory.path(),
        &[
            "index-pack",
            "--index-version=1",
            "-o",
            &replacement.to_string_lossy(),
            &location.path.to_string_lossy(),
        ],
        b"",
    )?;
    drop(location);
    #[cfg(windows)]
    {
        let mut permissions = std::fs::metadata(&index)?.permissions();
        permissions.set_readonly(false);
        std::fs::set_permissions(&index, permissions)?;
    }
    std::fs::remove_file(&index)?;
    std::fs::rename(replacement, index)?;
    let mut output = Vec::new();
    let scratch = || {
        Ok(ScratchSpace::new(
            fixture.directory.path(),
            8 * 1024 * 1024_u64,
        ))
    };
    assert!(copy_packed_blob(
        &fixture.objects,
        &fixture.oid,
        fixture.bytes.len() as u64,
        &mut output,
        scratch,
        &|| false
    )?);
    assert_eq!(output, fixture.bytes);
    Ok(())
}

#[test]
fn deep_native_delta_chain_uses_one_admitted_program_file() -> io::Result<()> {
    use gix_pack::data::entry::Header;
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    std::fs::write(root.join("gitconfig"), "")?;
    git(
        root,
        &["init", "--bare", "--template=", "--object-format=sha1"],
        b"",
    )?;
    let mut pack = b"PACK\0\0\0\x02".to_vec();
    pack.extend_from_slice(&129_u32.to_be_bytes());
    let mut body = b"original".to_vec();
    let mut previous = 0;
    for step in 0..129 {
        let offset = pack.len() as u64;
        let (header, bytes) = if step == 0 {
            (Header::Blob, body.clone())
        } else {
            let mut delta = Vec::new();
            for size in [body.len(), body.len() + 1] {
                let mut size = size;
                while size >= 128 {
                    delta.push((size as u8 & 127) | 128);
                    size >>= 7;
                }
                delta.push(size as u8);
            }
            delta.extend_from_slice(&[0x90, body.len() as u8, 1, step as u8]);
            body.push(step as u8);
            (
                Header::OfsDelta {
                    base_distance: offset - previous,
                },
                delta,
            )
        };
        header.write_to(bytes.len() as u64, &mut pack)?;
        let mut compressed =
            flate2::write::ZlibEncoder::new(&mut pack, flate2::Compression::default());
        compressed.write_all(&bytes)?;
        compressed.finish()?;
        previous = offset;
    }
    let mut hash = gix_hash::hasher(gix_hash::Kind::Sha1);
    hash.update(&pack);
    let checksum = hash.try_finalize().map_err(io::Error::other)?;
    pack.extend_from_slice(checksum.as_bytes());
    let path = root.join(format!("objects/pack/pack-{checksum}.pack"));
    std::fs::write(&path, pack)?;
    git(root, &["index-pack", &path.to_string_lossy()], b"")?;
    let mut hash = ObjectHasher::new(&"0".repeat(40))?;
    hash.update(format!("blob {}\0", body.len()).as_bytes());
    hash.update(&body);
    let oid = hash.finalize()?;
    let scratch = root.join("scratch");
    std::fs::create_dir(&scratch)?;
    let mut output = Vec::new();
    let space = || Ok(ScratchSpace::new(&scratch, 64 * 1024_u64));
    assert!(copy_packed_blob(
        &root.join("objects"),
        &oid,
        body.len() as u64,
        &mut output,
        space,
        &|| false
    )?);
    assert_eq!(output, body);
    assert_eq!(std::fs::read_dir(scratch)?.count(), 0);
    Ok(())
}
