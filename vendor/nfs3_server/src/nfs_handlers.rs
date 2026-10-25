#![allow(clippy::unwrap_used)] // FIXME: will fix this after some refactoring

#[allow(clippy::wildcard_imports)]
use nfs3_types::nfs3::*;
use nfs3_types::rpc::accept_stat_data;
use nfs3_types::xdr_codec::{BoundedList, Opaque, Pack, Unpack, Void};
use tracing::{debug, error, trace, warn};

use crate::context::RPCContext;
use crate::nfs_ext::{BoundedEntryPlusList, CookieVerfExt};
use crate::rpcwire::handle;
use crate::rpcwire::messages::{HandleResult, IncomingRpcMessage};
use crate::units::{GIBIBYTE, TEBIBYTE};
use crate::vfs::{NextResult, NfsFileSystem, VFSCapabilities};

#[allow(clippy::enum_glob_use)]
pub async fn handle_nfs<T>(
    context: RPCContext<T>,
    message: IncomingRpcMessage,
) -> anyhow::Result<HandleResult>
where
    T: NfsFileSystem,
{
    use NFS_PROGRAM::*;

    let call = message.body();
    let xid = message.xid();

    debug!("handle_nfs({xid}, {call:?}");
    if call.vers != VERSION {
        error!("Invalid NFSv3 Version number {} != {VERSION}", call.vers,);
        return message.into_error_reply(accept_stat_data::PROG_MISMATCH {
            low: VERSION,
            high: VERSION,
        });
    }

    let Ok(proc) = NFS_PROGRAM::try_from(call.proc) else {
        error!("invalid NFS3 Program number {}", call.proc);
        return message.into_error_reply(accept_stat_data::PROC_UNAVAIL);
    };

    debug!("{proc}({})", message.xid());
    match proc {
        NFSPROC3_NULL => handle(context, message, nfsproc3_null).await,
        NFSPROC3_GETATTR => handle(context, message, nfsproc3_getattr).await,
        NFSPROC3_LOOKUP => handle(context, message, nfsproc3_lookup).await,
        NFSPROC3_READ => handle(context, message, nfsproc3_read).await,
        NFSPROC3_FSINFO => handle(context, message, nfsproc3_fsinfo).await,
        NFSPROC3_ACCESS => handle(context, message, nfsproc3_access).await,
        NFSPROC3_PATHCONF => handle(context, message, nfsproc3_pathconf).await,
        NFSPROC3_FSSTAT => handle(context, message, nfsproc3_fsstat).await,
        NFSPROC3_READDIR => handle(context, message, nfsproc3_readdir).await,
        NFSPROC3_READDIRPLUS => handle(context, message, nfsproc3_readdirplus).await,
        NFSPROC3_WRITE => handle(context, message, nfsproc3_write).await,
        NFSPROC3_CREATE => handle(context, message, nfsproc3_create).await,
        NFSPROC3_SETATTR => handle(context, message, nfsproc3_setattr).await,
        NFSPROC3_REMOVE | NFSPROC3_RMDIR => handle(context, message, nfsproc3_remove).await,
        NFSPROC3_RENAME => handle(context, message, nfsproc3_rename).await,
        NFSPROC3_MKDIR => handle(context, message, nfsproc3_mkdir).await,
        NFSPROC3_SYMLINK => handle(context, message, nfsproc3_symlink).await,
        NFSPROC3_READLINK => handle(context, message, nfsproc3_readlink).await,
        NFSPROC3_COMMIT => handle(context, message, nfsproc3_commit).await,
        NFSPROC3_MKNOD | NFSPROC3_LINK => {
            warn!("Unimplemented message {proc}");
            message.into_error_reply(accept_stat_data::PROC_UNAVAIL)
        }
    }
}

macro_rules! fh_to_id {
    ($context:expr, $fh:expr) => {
        match $context.file_handle_converter.fh_from_nfs($fh) {
            Ok(id) => id,
            Err(stat) => {
                warn!("cannot resolve fh: {stat}");
                return Nfs3Result::Err((stat, Default::default()));
            }
        }
    };
}

async fn nfsproc3_null<T>(_: RPCContext<T>, _: u32, _: Void) -> Void
where
    T: crate::vfs::NfsFileSystem,
{
    Void
}

async fn nfsproc3_getattr<T>(
    context: RPCContext<T>,
    xid: u32,
    getattr3args: GETATTR3args,
) -> GETATTR3res
where
    T: NfsFileSystem,
{
    let handle = getattr3args.object;

    let id = fh_to_id!(context, &handle);
    match context.vfs.getattr(&id).await {
        Ok(obj_attributes) => {
            debug!(" {xid} --> {obj_attributes:?}");
            GETATTR3res::Ok(GETATTR3resok { obj_attributes })
        }
        Err(stat) => {
            warn!("getattr error {xid} --> {stat}");
            GETATTR3res::Err((stat, Void))
        }
    }
}

async fn nfsproc3_lookup<T>(
    context: RPCContext<T>,
    xid: u32,
    lookup3args: LOOKUP3args<'_>,
) -> LOOKUP3res
where
    T: NfsFileSystem,
{
    let dirops = lookup3args.what;
    let dirid = fh_to_id!(context, &dirops.dir);
    let dir_attributes = nfs_option_from_result(context.vfs.getattr(&dirid).await);
    match context.vfs.lookup(&dirid, &dirops.name).await {
        Ok(fid) => {
            let obj_attributes = nfs_option_from_result(context.vfs.getattr(&fid).await);
            debug!("lookup success {} --> {:?}", xid, obj_attributes);
            LOOKUP3res::Ok(LOOKUP3resok {
                object: context.file_handle_converter.fh_to_nfs(&fid),
                obj_attributes,
                dir_attributes,
            })
        }
        Err(stat) => {
            debug!("lookup error {xid}({:?}) --> {stat}", dirops.name,);
            LOOKUP3res::Err((stat, LOOKUP3resfail { dir_attributes }))
        }
    }
}

async fn nfsproc3_read<T>(
    context: RPCContext<T>,
    xid: u32,
    read3args: READ3args,
) -> READ3res<'static>
where
    T: NfsFileSystem,
{
    let handle = read3args.file;
    let id = fh_to_id!(context, &handle);
    let file_attributes = nfs_option_from_result(context.vfs.getattr(&id).await);
    match context
        .vfs
        .read(&id, read3args.offset, read3args.count)
        .await
    {
        Ok((bytes, eof)) => {
            debug!(" {xid} --> read {} bytes, eof: {eof}", bytes.len());
            READ3res::Ok(READ3resok {
                file_attributes,
                count: u32::try_from(bytes.len()).expect("buffer is too big"),
                eof,
                data: Opaque::owned(bytes),
            })
        }
        Err(stat) => {
            error!("read error {} --> {stat}", xid);
            READ3res::Err((stat, READ3resfail { file_attributes }))
        }
    }
}

async fn nfsproc3_fsinfo<T>(context: RPCContext<T>, xid: u32, args: FSINFO3args) -> FSINFO3res
where
    T: NfsFileSystem,
{
    let handle = args.fsroot;
    let id = fh_to_id!(context, &handle);
    match context.vfs.fsinfo(&id).await {
        Ok(fsinfo) => {
            debug!("fsinfo success {xid} --> {fsinfo:?}");
            FSINFO3res::Ok(fsinfo)
        }
        Err(stat) => {
            warn!("fsinfo error {xid} --> {stat}");
            FSINFO3res::Err((
                stat,
                FSINFO3resfail {
                    obj_attributes: post_op_attr::None,
                },
            ))
        }
    }
}

async fn nfsproc3_access<T>(context: RPCContext<T>, xid: u32, args: ACCESS3args) -> ACCESS3res
where
    T: NfsFileSystem,
{
    let handle = args.object;
    let mut access = args.access;
    let id = fh_to_id!(context, &handle);
    let obj_attributes = nfs_option_from_result(context.vfs.getattr(&id).await);

    if !matches!(context.vfs.capabilities(), VFSCapabilities::ReadWrite) {
        access &= ACCESS3_READ | ACCESS3_LOOKUP | ACCESS3_EXECUTE;
    }

    debug!("access success {xid} --> {access:?}");
    ACCESS3res::Ok(ACCESS3resok {
        obj_attributes,
        access,
    })
}

async fn nfsproc3_pathconf<T>(context: RPCContext<T>, xid: u32, args: PATHCONF3args) -> PATHCONF3res
where
    T: NfsFileSystem,
{
    let handle = args.object;
    debug!("nfsproc3_pathconf({xid}, {handle:?})");
    let id = fh_to_id!(context, &handle);
    let obj_attr = nfs_option_from_result(context.vfs.getattr(&id).await);

    let res = PATHCONF3resok {
        obj_attributes: obj_attr,
        linkmax: 0,
        name_max: 32768,
        no_trunc: true,
        chown_restricted: true,
        case_insensitive: false,
        case_preserving: true,
    };

    debug!("pathconf success {xid} --> {res:?}");
    PATHCONF3res::Ok(res)
}

async fn nfsproc3_fsstat<T>(context: RPCContext<T>, xid: u32, args: FSSTAT3args) -> FSSTAT3res
where
    T: NfsFileSystem,
{
    let handle = args.fsroot;
    let id = fh_to_id!(context, &handle);
    let obj_attr = nfs_option_from_result(context.vfs.getattr(&id).await);
    let fsstat = FSSTAT3resok {
        obj_attributes: obj_attr,
        tbytes: TEBIBYTE,
        fbytes: TEBIBYTE,
        abytes: TEBIBYTE,
        tfiles: GIBIBYTE,
        ffiles: GIBIBYTE,
        afiles: GIBIBYTE,
        invarsec: u32::MAX,
    };

    debug!("fsstat success {xid} --> {fsstat:?}");
    FSSTAT3res::Ok(fsstat)
}

async fn nfsproc3_readdirplus<T>(
    context: RPCContext<T>,
    xid: u32,
    args: READDIRPLUS3args,
) -> READDIRPLUS3res<'static>
where
    T: NfsFileSystem,
{
    use crate::vfs::ReadDirPlusIterator;

    let dirid = fh_to_id!(context, &args.dir);
    let dir_attr_maybe = context.vfs.getattr(&dirid).await;

    let dir_attributes = dir_attr_maybe.map_or(post_op_attr::None, post_op_attr::Some);

    let dirversion = cookieverf3::from_attr(&dir_attributes);
    debug!(" -- Dir attr {dir_attributes:?}");
    debug!(" -- Dir version {dirversion:?}");
    let has_version = args.cookieverf.is_some();
    // initial call should have empty cookie verf
    // subsequent calls should have cvf_version as defined above
    // which is based off the mtime.
    //
    // TODO: This is *far* too aggressive. and unnecessary.
    // The client should maintain this correctly typically.
    //
    // The way cookieverf is handled is quite interesting...
    //
    // There are 2 notes in the RFC of interest:
    // 1. If the
    // server detects that the cookie is no longer valid, the
    // server will reject the READDIR request with the status,
    // NFS3ERR_BAD_COOKIE. The client should be careful to
    // avoid holding directory entry cookies across operations
    // that modify the directory contents, such as REMOVE and
    // CREATE.
    //
    // 2. One implementation of the cookie-verifier mechanism might
    //  be for the server to use the modification time of the
    //  directory. This might be overly restrictive, however. A
    //  better approach would be to record the time of the last
    //  directory modification that changed the directory
    //  organization in a way that would make it impossible to
    //  reliably interpret a cookie. Servers in which directory
    //  cookies are always valid are free to use zero as the
    //  verifier always.
    //
    //  Basically, as long as the cookie is "kinda" intepretable,
    //  we should keep accepting it.
    //  On testing, the Mac NFS client pretty much expects that
    //  especially on highly concurrent modifications to the directory.
    //
    //  1. If part way through a directory enumeration we fail with BAD_COOKIE
    //  if the directory contents change, the client listing may fail resulting
    //  in a "no such file or directory" error.
    //  2. if we cache readdir results. i.e. we think of a readdir as two parts a. enumerating
    //     everything first b. the cookie is then used to paginate the enumeration we can run into
    //     file time synchronization issues. i.e. while one listing occurs and another file is
    //     touched, the listing may report an outdated file status.
    //
    //     This cache also appears to have to be *quite* long lasting
    //     as the client may hold on to a directory enumerator
    //     with unbounded time.
    //
    //  Basically, if we think about how linux directory listing works
    //  is that you just get an enumerator. There is no mechanic available for
    //  "restarting" a pagination and this enumerator is assumed to be valid
    //  even across directory modifications and should reflect changes
    //  immediately.
    //
    //  The best solution is simply to really completely avoid sending
    //  BAD_COOKIE all together and to ignore the cookie mechanism.
    //
    // if args.cookieverf != cookieverf3::default() && args.cookieverf != dirversion {
    // info!(" -- Dir version mismatch. Received {:?}", args.cookieverf);
    // make_success_reply(xid).pack(output)?;
    // nfsstat3::NFS3ERR_BAD_COOKIE.pack(output)?;
    // dir_attr.pack(output)?;
    // return Ok(());
    // }

    // subtract off the final entryplus* field (which must be false) and the eof
    if args.maxcount < 128 {
        // we have no space to write anything
        let stat = nfsstat3::NFS3ERR_TOOSMALL;
        error!("readdirplus error {xid} --> {stat}");
        return READDIRPLUS3res::Err((stat, READDIRPLUS3resfail { dir_attributes }));
    }
    let max_bytes_allowed = args.maxcount as usize - 128;

    let iter = context.vfs.readdirplus(&dirid, args.cookie).await;

    if let Err(stat) = iter {
        error!("readdirplus error {xid} --> {stat}");
        return READDIRPLUS3res::Err((stat, READDIRPLUS3resfail { dir_attributes }));
    }

    let mut iter = iter.unwrap();
    let eof;

    // this is a wrapper around a writer that also just counts the number of bytes
    // written
    let mut entries_result = BoundedEntryPlusList::new(args.dircount as usize, max_bytes_allowed);
    loop {
        match iter.next().await {
            NextResult::Ok(dir_entry_plus) => {
                let entry = dir_entry_plus.into_entry(&context.file_handle_converter);
                let result = entries_result.try_push(entry);
                if result.is_err() {
                    trace!(" -- insufficient space. truncating");
                    eof = false;
                    break;
                }
            }
            NextResult::Eof => {
                eof = true;
                break;
            }
            NextResult::Err(stat) => {
                error!("readdirplus error {xid} --> {stat}");
                return READDIRPLUS3res::Err((stat, READDIRPLUS3resfail { dir_attributes }));
            }
        }
    }

    let entries = entries_result.into_inner();
    if entries.0.is_empty() && !eof {
        let stat = nfsstat3::NFS3ERR_TOOSMALL;
        error!("readdirplus error {xid} --> {stat}");
        return READDIRPLUS3res::Err((stat, READDIRPLUS3resfail { dir_attributes }));
    }

    debug!("  -- readdirplus eof {eof}");
    debug!(
        "readdirplus {dirid:?}, has_version {has_version}, start at {}, flushing {} entries, \
         complete {eof}",
        args.cookie,
        entries.0.len()
    );

    READDIRPLUS3res::Ok(READDIRPLUS3resok {
        dir_attributes,
        cookieverf: dirversion,
        reply: dirlistplus3 { entries, eof },
    })
}

#[allow(clippy::too_many_lines)]
async fn nfsproc3_readdir<T>(
    context: RPCContext<T>,
    xid: u32,
    readdir3args: READDIR3args,
) -> READDIR3res<'static>
where
    T: NfsFileSystem,
{
    use crate::vfs::ReadDirIterator;

    let dirid = fh_to_id!(context, &readdir3args.dir);
    let dir_attr_maybe = context.vfs.getattr(&dirid).await;
    let dir_attributes = dir_attr_maybe.map_or(post_op_attr::None, post_op_attr::Some);
    let cookieverf = cookieverf3::from_attr(&dir_attributes);

    if readdir3args.cookieverf.is_none() {
        if readdir3args.cookie != 0 {
            warn!(
                " -- Invalid cookie. Expected 0, got {}",
                readdir3args.cookie
            );
            return READDIR3res::Err((nfsstat3::NFS3ERR_BAD_COOKIE, READDIR3resfail::default()));
        }
        debug!(" -- Start of readdir");
    } else if readdir3args.cookieverf != cookieverf {
        warn!(
            " -- Dir version mismatch. Received {:?}, Expected: {cookieverf:?}",
            readdir3args.cookieverf,
        );
        return READDIR3res::Err((nfsstat3::NFS3ERR_BAD_COOKIE, READDIR3resfail::default()));
    } else {
        debug!(" -- Resuming readdir. Cookie {}", readdir3args.cookie);
    }

    debug!(" -- Dir attr {dir_attributes:?}");
    debug!(" -- Dir version {cookieverf:?}");

    let mut resok = READDIR3resok {
        dir_attributes,
        cookieverf,
        reply: dirlist3::default(),
    };

    let empty_len = xid.packed_size() + resok.packed_size();
    if empty_len > readdir3args.count as usize {
        // we have no space to write anything
        return READDIR3res::Err((
            nfsstat3::NFS3ERR_TOOSMALL,
            READDIR3resfail {
                dir_attributes: resok.dir_attributes,
            },
        ));
    }
    let max_bytes_allowed = readdir3args.count as usize - empty_len;

    let iter = context.vfs.readdir(&dirid, readdir3args.cookie).await;
    if let Err(stat) = iter {
        return READDIR3res::Err((
            stat,
            READDIR3resfail {
                dir_attributes: resok.dir_attributes,
            },
        ));
    }

    let mut iter = iter.unwrap();
    let mut entries = BoundedList::new(max_bytes_allowed);
    let eof;
    loop {
        match iter.next().await {
            NextResult::Ok(entry) => {
                let result = entries.try_push(entry);
                if result.is_err() {
                    trace!(" -- insufficient space. truncating");
                    eof = false;
                    break;
                }
            }
            NextResult::Eof => {
                eof = true;
                break;
            }
            NextResult::Err(stat) => {
                error!("readdir error {xid} --> {stat}");
                return READDIR3res::Err((
                    stat,
                    READDIR3resfail {
                        dir_attributes: resok.dir_attributes,
                    },
                ));
            }
        }
    }

    let entries = entries.into_inner();
    if entries.is_empty() && !eof {
        let stat = nfsstat3::NFS3ERR_TOOSMALL;
        error!("readdir error {xid} --> {stat}");
        return READDIR3res::Err((
            stat,
            READDIR3resfail {
                dir_attributes: resok.dir_attributes,
            },
        ));
    }

    resok.reply.entries = entries;
    resok.reply.eof = eof;
    Nfs3Result::Ok(resok)
}

async fn nfsproc3_write<T>(
    context: RPCContext<T>,
    xid: u32,
    write3args: WRITE3args<'_>,
) -> WRITE3res
where
    T: NfsFileSystem,
{
    if !matches!(context.vfs.capabilities(), VFSCapabilities::ReadWrite) {
        warn!("No write capabilities.");
        return WRITE3res::Err((nfsstat3::NFS3ERR_ROFS, WRITE3resfail::default()));
    }

    if write3args.data.len() != write3args.count as usize {
        error!(
            "Data length mismatch: expected {}, got {}",
            write3args.count,
            write3args.data.len()
        );
        return WRITE3res::Err((nfsstat3::NFS3ERR_INVAL, WRITE3resfail::default()));
    }

    let id = fh_to_id!(context, &write3args.file);
    let before = get_wcc_attr(&context, &id)
        .await
        .map_or(pre_op_attr::None, pre_op_attr::Some);

    match context
        .vfs
        .write(&id, write3args.offset, &write3args.data, write3args.stable)
        .await
    {
        Ok((fattr, committed)) => {
            debug!("write success {xid} --> {fattr:?}, committed: {committed}");
            WRITE3res::Ok(WRITE3resok {
                file_wcc: wcc_data {
                    before,
                    after: post_op_attr::Some(fattr),
                },
                count: write3args.count,
                committed,
                verf: context.file_handle_converter.verf(),
            })
        }
        Err(stat) => {
            error!("write error {xid} --> {stat}");
            WRITE3res::Err((
                stat,
                WRITE3resfail {
                    file_wcc: wcc_data {
                        before,
                        after: post_op_attr::None,
                    },
                },
            ))
        }
    }
}

async fn nfsproc3_commit<T>(context: RPCContext<T>, xid: u32, args: COMMIT3args) -> COMMIT3res
where
    T: NfsFileSystem,
{
    let id = match context.file_handle_converter.fh_from_nfs(&args.file) {
        Ok(id) => id,
        Err(stat) => {
            warn!("cannot resolve fh: {stat}");
            return COMMIT3res::Err((
                stat,
                COMMIT3resfail {
                    file_wcc: wcc_data::default(),
                },
            ));
        }
    };
    let before = get_wcc_attr(&context, &id)
        .await
        .map_or(pre_op_attr::None, pre_op_attr::Some);
    match context.vfs.commit(&id, args.offset, args.count).await {
        Ok(()) => {
            let after = nfs_option_from_result(context.vfs.getattr(&id).await);
            debug!("commit success {xid}");
            COMMIT3res::Ok(COMMIT3resok {
                file_wcc: wcc_data { before, after },
                verf: context.file_handle_converter.verf(),
            })
        }
        Err(stat) => {
            let after = nfs_option_from_result(context.vfs.getattr(&id).await);
            error!("commit error {xid} --> {stat}");
            COMMIT3res::Err((
                stat,
                COMMIT3resfail {
                    file_wcc: wcc_data { before, after },
                },
            ))
        }
    }
}

#[allow(clippy::collapsible_if, clippy::too_many_lines)]
async fn nfsproc3_create<T>(context: RPCContext<T>, xid: u32, args: CREATE3args<'_>) -> CREATE3res
where
    T: NfsFileSystem,
{
    if !matches!(context.vfs.capabilities(), VFSCapabilities::ReadWrite) {
        warn!("No write capabilities.");
        return CREATE3res::Err((nfsstat3::NFS3ERR_ROFS, CREATE3resfail::default()));
    }

    let dirops = args.where_;
    let createhow = args.how;

    debug!("nfsproc3_create({xid}, {dirops:?}, {createhow:?})");
    let dirid = fh_to_id!(context, &dirops.dir);
    // get the object attributes before the write
    let before = match get_wcc_attr(&context, &dirid).await {
        Ok(wccattr) => pre_op_attr::Some(wccattr),
        Err(stat) => {
            warn!("Cannot stat directory {xid} -> {stat}");
            return CREATE3res::Err((stat, CREATE3resfail::default()));
        }
    };

    if matches!(&createhow, createhow3::GUARDED(_)) {
        if context.vfs.lookup(&dirid, &dirops.name).await.is_ok() {
            let after = nfs_option_from_result(context.vfs.getattr(&dirid).await);
            return CREATE3res::Err((
                nfsstat3::NFS3ERR_EXIST,
                CREATE3resfail {
                    dir_wcc: wcc_data { before, after },
                },
            ));
        }
    }

    let (fid, postopattr) = match createhow {
        createhow3::EXCLUSIVE(verf) => {
            let fid = context
                .vfs
                .create_exclusive(&dirid, &dirops.name, verf)
                .await;
            (fid, post_op_attr::None)
        }
        createhow3::UNCHECKED(target_attributes) | createhow3::GUARDED(target_attributes) => {
            match context
                .vfs
                .create(&dirid, &dirops.name, target_attributes)
                .await
            {
                Ok((fid, fattr)) => (Ok(fid), post_op_attr::Some(fattr)),
                Err(e) => (Err(e), post_op_attr::None),
            }
        }
    };

    let after = nfs_option_from_result(context.vfs.getattr(&dirid).await);
    let dir_wcc = wcc_data { before, after };

    match fid {
        Ok(fid) => {
            debug!("create success {xid} --> {fid:?}, {postopattr:?}");
            CREATE3res::Ok(CREATE3resok {
                obj: post_op_fh3::Some(context.file_handle_converter.fh_to_nfs(&fid)),
                obj_attributes: postopattr,
                dir_wcc,
            })
        }
        Err(stat) => {
            error!("create error {xid} --> {stat}");
            CREATE3res::Err((stat, CREATE3resfail { dir_wcc }))
        }
    }
}

async fn nfsproc3_setattr<T>(context: RPCContext<T>, xid: u32, args: SETATTR3args) -> SETATTR3res
where
    T: NfsFileSystem,
{
    if !matches!(context.vfs.capabilities(), VFSCapabilities::ReadWrite) {
        warn!("No write capabilities.");
        return SETATTR3res::Err((nfsstat3::NFS3ERR_ROFS, SETATTR3resfail::default()));
    }

    let id = fh_to_id!(context, &args.object);
    let ctime;
    let before = match get_wcc_attr(&context, &id).await {
        Ok(wccattr) => {
            ctime = wccattr.ctime;
            pre_op_attr::Some(wccattr)
        }
        Err(stat) => {
            warn!("Cannot stat object {xid} --> {stat}");
            return SETATTR3res::Err((stat, SETATTR3resfail::default()));
        }
    };

    if let sattrguard3::Some(guard) = args.guard {
        if guard != ctime {
            warn!("setattr guard mismatch {xid}");
            return SETATTR3res::Err((
                nfsstat3::NFS3ERR_NOT_SYNC,
                SETATTR3resfail {
                    obj_wcc: wcc_data {
                        before,
                        after: post_op_attr::None,
                    },
                },
            ));
        }
    }

    match context.vfs.setattr(&id, args.new_attributes).await {
        Ok(post_op_attr) => {
            debug!("setattr success {xid} --> {post_op_attr:?}");
            SETATTR3res::Ok(SETATTR3resok {
                obj_wcc: wcc_data {
                    before,
                    after: post_op_attr::Some(post_op_attr),
                },
            })
        }
        Err(stat) => {
            error!("setattr error {xid} --> {stat}");
            SETATTR3res::Err((
                stat,
                SETATTR3resfail {
                    obj_wcc: wcc_data {
                        before,
                        after: post_op_attr::None,
                    },
                },
            ))
        }
    }
}

async fn nfsproc3_remove<T>(context: RPCContext<T>, xid: u32, args: REMOVE3args<'_>) -> REMOVE3res
where
    T: NfsFileSystem,
{
    if !matches!(context.vfs.capabilities(), VFSCapabilities::ReadWrite) {
        warn!("No write capabilities.");
        return REMOVE3res::Err((nfsstat3::NFS3ERR_ROFS, REMOVE3resfail::default()));
    }

    let dirid = fh_to_id!(context, &args.object.dir);
    let before = match get_wcc_attr(&context, &dirid).await {
        Ok(v) => pre_op_attr::Some(v),
        Err(stat) => {
            warn!("Cannot stat directory {xid} -> {stat}");
            return REMOVE3res::Err((stat, REMOVE3resfail::default()));
        }
    };

    match context.vfs.remove(&dirid, &args.object.name).await {
        Ok(()) => {
            let after = nfs_option_from_result(context.vfs.getattr(&dirid).await);
            debug!("remove success {xid}");
            REMOVE3res::Ok(REMOVE3resok {
                dir_wcc: wcc_data { before, after },
            })
        }
        Err(stat) => {
            let after = nfs_option_from_result(context.vfs.getattr(&dirid).await);
            error!("remove error {xid} --> {stat}");
            REMOVE3res::Err((
                stat,
                REMOVE3resfail {
                    dir_wcc: wcc_data { before, after },
                },
            ))
        }
    }
}

async fn nfsproc3_rename<T>(
    context: RPCContext<T>,
    xid: u32,
    args: RENAME3args<'_, '_>,
) -> RENAME3res
where
    T: NfsFileSystem,
{
    if !matches!(context.vfs.capabilities(), VFSCapabilities::ReadWrite) {
        warn!("No write capabilities.");
        return RENAME3res::Err((nfsstat3::NFS3ERR_ROFS, RENAME3resfail::default()));
    }

    let from_dirid = fh_to_id!(context, &args.from.dir);
    let to_dirid = fh_to_id!(context, &args.to.dir);
    let pre_from_dir_attr = match get_wcc_attr(&context, &from_dirid).await {
        Ok(v) => pre_op_attr::Some(v),
        Err(stat) => {
            warn!("Cannot stat source directory {xid} --> {stat}");
            return RENAME3res::Err((stat, RENAME3resfail::default()));
        }
    };

    let pre_to_dir_attr = match get_wcc_attr(&context, &to_dirid).await {
        Ok(v) => pre_op_attr::Some(v),
        Err(stat) => {
            warn!("Cannot stat target directory {xid} --> {stat}");
            return RENAME3res::Err((stat, RENAME3resfail::default()));
        }
    };

    let result = context
        .vfs
        .rename(&from_dirid, &args.from.name, &to_dirid, &args.to.name)
        .await;

    let post_from_dir_attr = nfs_option_from_result(context.vfs.getattr(&from_dirid).await);
    let post_to_dir_attr = nfs_option_from_result(context.vfs.getattr(&to_dirid).await);

    let fromdir_wcc = wcc_data {
        before: pre_from_dir_attr,
        after: post_from_dir_attr,
    };
    let todir_wcc = wcc_data {
        before: pre_to_dir_attr,
        after: post_to_dir_attr,
    };
    match result {
        Ok(()) => {
            debug!("rename success {xid}");
            RENAME3res::Ok(RENAME3resok {
                fromdir_wcc,
                todir_wcc,
            })
        }
        Err(stat) => {
            error!("rename error {xid} --> {stat}");
            RENAME3res::Err((
                stat,
                RENAME3resfail {
                    fromdir_wcc,
                    todir_wcc,
                },
            ))
        }
    }
}
async fn nfsproc3_mkdir<T>(context: RPCContext<T>, xid: u32, args: MKDIR3args<'_>) -> MKDIR3res
where
    T: NfsFileSystem,
{
    if !matches!(context.vfs.capabilities(), VFSCapabilities::ReadWrite) {
        warn!("No write capabilities.");
        return MKDIR3res::Err((nfsstat3::NFS3ERR_ROFS, MKDIR3resfail::default()));
    }

    let dirid = fh_to_id!(context, &args.where_.dir);

    let before = match get_wcc_attr(&context, &dirid).await {
        Ok(v) => pre_op_attr::Some(v),
        Err(stat) => {
            warn!("Cannot stat directory {xid} --> {stat}");
            return MKDIR3res::Err((stat, MKDIR3resfail::default()));
        }
    };

    let result = context.vfs.mkdir(&dirid, &args.where_.name).await;
    let after = nfs_option_from_result(context.vfs.getattr(&dirid).await);
    let dir_wcc = wcc_data { before, after };

    match result {
        Ok((fid, fattr)) => {
            debug!("mkdir success {xid} --> {fid:?}, {fattr:?}");
            MKDIR3res::Ok(MKDIR3resok {
                obj: post_op_fh3::Some(context.file_handle_converter.fh_to_nfs(&fid)),
                obj_attributes: post_op_attr::Some(fattr),
                dir_wcc,
            })
        }
        Err(stat) => {
            error!("mkdir error {xid} --> {stat}");
            MKDIR3res::Err((stat, MKDIR3resfail { dir_wcc }))
        }
    }
}

async fn nfsproc3_symlink<T>(
    context: RPCContext<T>,
    xid: u32,
    args: SYMLINK3args<'_>,
) -> SYMLINK3res
where
    T: NfsFileSystem,
{
    if !matches!(context.vfs.capabilities(), VFSCapabilities::ReadWrite) {
        warn!("No write capabilities.");
        return SYMLINK3res::Err((nfsstat3::NFS3ERR_ROFS, SYMLINK3resfail::default()));
    }

    let dirid = fh_to_id!(context, &args.where_.dir);

    let pre_dir_attr = match get_wcc_attr(&context, &dirid).await {
        Ok(v) => pre_op_attr::Some(v),
        Err(stat) => {
            warn!("Cannot stat directory {xid} --> {stat}");
            return SYMLINK3res::Err((stat, SYMLINK3resfail::default()));
        }
    };

    match context
        .vfs
        .symlink(
            &dirid,
            &args.where_.name,
            &args.symlink.symlink_data,
            &args.symlink.symlink_attributes,
        )
        .await
    {
        Ok((fid, fattr)) => {
            debug!("symlink success {xid} --> {fid:?}, {fattr:?}");
            SYMLINK3res::Ok(SYMLINK3resok {
                obj: post_op_fh3::Some(context.file_handle_converter.fh_to_nfs(&fid)),
                obj_attributes: post_op_attr::Some(fattr),
                dir_wcc: wcc_data {
                    before: pre_dir_attr,
                    after: nfs_option_from_result(context.vfs.getattr(&dirid).await),
                },
            })
        }
        Err(stat) => {
            error!("symlink error {xid} --> {stat}");
            SYMLINK3res::Err((
                stat,
                SYMLINK3resfail {
                    dir_wcc: wcc_data {
                        before: pre_dir_attr,
                        after: nfs_option_from_result(context.vfs.getattr(&dirid).await),
                    },
                },
            ))
        }
    }
}

async fn nfsproc3_readlink<T>(
    context: RPCContext<T>,
    xid: u32,
    args: READLINK3args,
) -> READLINK3res<'static>
where
    T: NfsFileSystem,
{
    let id = fh_to_id!(context, &args.symlink);
    let symlink_attributes = nfs_option_from_result(context.vfs.getattr(&id).await);

    match context.vfs.readlink(&id).await {
        Ok(data) => {
            debug!("readlink success {xid} --> {data:?}");
            READLINK3res::Ok(READLINK3resok {
                symlink_attributes,
                data: data.into_owned(),
            })
        }
        Err(stat) => {
            error!("readlink error {xid} --> {stat}");
            READLINK3res::Err((stat, READLINK3resfail { symlink_attributes }))
        }
    }
}

fn nfs_option_from_result<T: Pack + Unpack, E>(result: Result<T, E>) -> Nfs3Option<T> {
    result.map_or(Nfs3Option::None, Nfs3Option::Some)
}

async fn get_wcc_attr<T>(
    context: &RPCContext<T>,
    object_id: &T::Handle,
) -> Result<wcc_attr, nfsstat3>
where
    T: NfsFileSystem,
{
    context.vfs.getattr(object_id).await.map(|v| wcc_attr {
        size: v.size,
        mtime: v.mtime,
        ctime: v.ctime,
    })
}
