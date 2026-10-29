use std::path::Path;

use crate::Result;

pub(crate) fn contains(mountpoint: &Path) -> Result<bool> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        let mounts = std::fs::read("/proc/self/mounts")?;
        Ok(linux_contains(&mounts, mountpoint.as_os_str().as_bytes()))
    }
    #[cfg(target_os = "macos")]
    {
        use std::ffi::CStr;
        use std::os::unix::ffi::OsStrExt;
        for _ in 0..3 {
            // MNT_NOWAIT reads the kernel table without entering the filesystem
            // whose callbacks this owner may be draining.
            // SAFETY: a null buffer asks only for the current entry count.
            let count = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
            if count < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let capacity = count as usize + 16;
            let bytes = capacity
                .checked_mul(std::mem::size_of::<libc::statfs>())
                .and_then(|bytes| i32::try_from(bytes).ok())
                .ok_or_else(|| std::io::Error::other("mount table exceeds query capacity"))?;
            let mut mounts = Vec::<libc::statfs>::with_capacity(capacity);
            // SAFETY: getfsstat initializes at most the buffer's capacity; the
            // kernel returns NUL-terminated names in initialized statfs entries.
            let count = unsafe { libc::getfsstat(mounts.as_mut_ptr(), bytes, libc::MNT_NOWAIT) };
            if count < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            if count as usize >= capacity {
                continue;
            }
            // SAFETY: only the returned, capacity-checked entries are initialized.
            unsafe {
                mounts.set_len(count as usize);
            }
            return Ok(mounts.iter().any(|stat| {
                // SAFETY: f_mntonname is a kernel-provided NUL-terminated field.
                let target = unsafe { CStr::from_ptr(stat.f_mntonname.as_ptr()) };
                target.to_bytes() == mountpoint.as_os_str().as_bytes()
            }));
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            "mount table changed during query",
        )
        .into())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = mountpoint;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "native mount table is unavailable",
        )
        .into())
    }
}

#[cfg(any(target_os = "linux", test))]
fn linux_contains(mounts: &[u8], mountpoint: &[u8]) -> bool {
    mounts.split(|byte| *byte == b'\n').any(|line| {
        line.split(|byte| *byte == b' ')
            .filter(|field| !field.is_empty())
            .nth(1)
            .is_some_and(|field| decode_field(field) == mountpoint)
    })
}

#[cfg(any(target_os = "linux", test))]
fn decode_field(field: &[u8]) -> Vec<u8> {
    let mut decoded = Vec::with_capacity(field.len());
    let mut index = 0;
    while index < field.len() {
        if field[index] == b'\\' && index + 3 < field.len() {
            let digits = &field[index + 1..index + 4];
            if digits.iter().all(|digit| (b'0'..=b'7').contains(digit)) {
                let value = digits
                    .iter()
                    .fold(0u16, |value, digit| value * 8 + u16::from(digit - b'0'));
                if let Ok(value) = u8::try_from(value) {
                    decoded.push(value);
                    index += 4;
                    continue;
                }
            }
        }
        decoded.push(field[index]);
        index += 1;
    }
    decoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_table_matches_exact_decoded_path_bytes() {
        let mounts =
            b"source /mnt/Crab\\040Mount\\011One nfs rw 0 0\nsource /mnt/raw\xff fuse rw 0 0\nsource /mnt/control\x0bname\x0c ext4 rw 0 0\n";
        for (path, expected) in [
            (b"/mnt/Crab Mount\tOne".as_slice(), true),
            (b"/mnt/Crab Mount\t".as_slice(), false),
            (b"/mnt/raw\xff".as_slice(), true),
            (b"/mnt/control\x0bname\x0c".as_slice(), true),
            (b"/mnt/raw\xef\xbf\xbd".as_slice(), false),
        ] {
            assert_eq!(linux_contains(mounts, path), expected);
        }
    }

    #[test]
    fn field_decoder_preserves_invalid_escapes() {
        for (field, expected) in [
            (br"a\040b".as_slice(), b"a b".as_slice()),
            (br"a\999b".as_slice(), br"a\999b".as_slice()),
            (br"a\04".as_slice(), br"a\04".as_slice()),
        ] {
            assert_eq!(decode_field(field), expected);
        }
    }
}
