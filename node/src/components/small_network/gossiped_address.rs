use std::{
    fmt::{self, Display, Formatter},
    net::SocketAddr,
};

use datasize::DataSize;
use serde::{Deserialize, Serialize};

use crate::{
    effect::GossipTarget,
    types::{GossiperItem, Item, Tag},
};

/// Used to gossip our public listening address to peers.
#[derive(
    Copy, Clone, DataSize, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize, Debug,
)]
pub struct GossipedAddress(SocketAddr);

impl GossipedAddress {
    pub(super) fn new(address: SocketAddr) -> Self {
        GossipedAddress(address)
    }
}

impl Display for GossipedAddress {
    fn fmt(&self, formatter: &mut Formatter) -> fmt::Result {
        write!(formatter, "gossiped-address {}", self.0)
    }
}

impl Item for GossipedAddress {
    type Id = GossipedAddress;
    const TAG: Tag = Tag::GossipedAddress;

    fn id(&self) -> Self::Id {
        *self
    }
}

impl GossiperItem for GossipedAddress {
    const ID_IS_COMPLETE_ITEM: bool = true;

    fn target(&self) -> GossipTarget {
        GossipTarget::All
    }
}

impl From<GossipedAddress> for SocketAddr {
    fn from(gossiped_address: GossipedAddress) -> Self {
        gossiped_address.0
    }
}
