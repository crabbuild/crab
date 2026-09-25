use crate::{CellsConfig, Error, Result};

pub(crate) use crab_cell_peer_http::{LoadedPeerTls, PeerTlsClient, PeerTlsIdentity};

pub(crate) fn load_peer_tls(cells: &CellsConfig) -> Result<LoadedPeerTls> {
    let server_name = cells
        .peer_tls_server_name
        .as_deref()
        .or_else(|| cells.peer_advertise.host_str())
        .ok_or(Error::Config("cells.peer_advertise has no host"))?;
    LoadedPeerTls::load(
        &cells.peer_certificate,
        &cells.peer_private_key,
        &cells.peer_ca,
        server_name,
    )
    .map_err(Into::into)
}

#[cfg(test)]
pub(crate) mod tests;
