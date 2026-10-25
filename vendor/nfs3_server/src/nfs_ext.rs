use nfs3_types::nfs3::{cookieverf3, entryplus3, post_op_attr};
use nfs3_types::xdr_codec::{BoundedList, List, Pack};

pub trait CookieVerfExt {
    const NONE_COOKIE_VERF: cookieverf3 = cookieverf3(0u64.to_be_bytes());
    const SOME_COOKIE_VERF: cookieverf3 = cookieverf3(0xFFCC_FFCC_FFCC_FFCCu64.to_be_bytes());

    fn from_attr(dir_attr: &post_op_attr) -> Self;
    fn is_none(&self) -> bool;
    fn is_some(&self) -> bool;
}

impl CookieVerfExt for cookieverf3 {
    fn from_attr(dir_attr: &post_op_attr) -> Self {
        if let post_op_attr::Some(attr) = dir_attr {
            let cvf_version =
                (u64::from(attr.mtime.seconds) << 32) | u64::from(attr.mtime.nseconds);
            Self(cvf_version.to_be_bytes())
        } else {
            Self::SOME_COOKIE_VERF
        }
    }

    fn is_none(&self) -> bool {
        self == &Self::NONE_COOKIE_VERF
    }

    fn is_some(&self) -> bool {
        !self.is_none()
    }
}
pub struct BoundedEntryPlusList {
    entries: BoundedList<entryplus3<'static>>,
    dircount: usize,
    accumulated_dircount: usize,
}

impl BoundedEntryPlusList {
    pub fn new(dircount: usize, maxcount: usize) -> Self {
        Self {
            entries: BoundedList::new(maxcount),
            dircount,
            accumulated_dircount: 0,
        }
    }

    #[allow(clippy::result_large_err)]
    pub fn try_push(&mut self, entry: entryplus3<'static>) -> Result<(), entryplus3<'static>> {
        // dircount - the maximum number of bytes of directory information returned. This number
        // should not include the size of the attributes and file handle portions of the result.
        let added_dircount =
            entry.fileid.packed_size() + 4 + entry.name.packed_size() + entry.cookie.packed_size();

        if self.accumulated_dircount + added_dircount > self.dircount {
            return Err(entry);
        }

        let result = self.entries.try_push(entry);
        if result.is_ok() {
            self.accumulated_dircount += added_dircount;
        }
        result
    }

    pub fn into_inner(self) -> List<entryplus3<'static>> {
        self.entries.into_inner()
    }
}

#[cfg(test)]
mod tests {
    use nfs3_types::nfs3::{filename3, post_op_fh3};
    use nfs3_types::xdr_codec::Opaque;

    use super::*;

    #[test]
    fn test_dircount() {
        let mut list = BoundedEntryPlusList::new(70, 1000);
        assert!(list.try_push(make_entry("test")).is_ok());
        assert_eq!(list.accumulated_dircount, 28);
        assert!(list.try_push(make_entry("test2")).is_ok());
        assert_eq!(list.accumulated_dircount, 60);
        assert!(list.try_push(make_entry("test3")).is_err());
        assert_eq!(list.accumulated_dircount, 60);

        let entries = list.into_inner();
        assert!(entries.packed_size() < 1000);
        assert_eq!(entries.0.len(), 2);
    }

    #[test]
    fn test_maxcount() {
        let mut list = BoundedEntryPlusList::new(1000, 100);
        assert!(list.try_push(make_entry("test")).is_ok());
        assert_eq!(list.accumulated_dircount, 28);
        assert!(list.try_push(make_entry("test2")).is_ok());
        assert_eq!(list.accumulated_dircount, 60);
        assert!(list.try_push(make_entry("test3")).is_err());
        assert_eq!(list.accumulated_dircount, 60);

        let entries = list.into_inner();
        assert_eq!(entries.packed_size(), 80);
        assert_eq!(entries.0.len(), 2);
    }

    fn make_entry(name: &str) -> entryplus3<'_> {
        entryplus3 {
            fileid: 0,
            name: filename3(Opaque::borrowed(name.as_bytes())),
            cookie: 0,
            name_attributes: post_op_attr::None,
            name_handle: post_op_fh3::None,
        }
    }
}
