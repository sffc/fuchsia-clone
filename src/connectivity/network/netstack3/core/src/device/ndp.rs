// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The Neighbor Discovery Protocol (NDP).
//!
//! Neighbor Discovery for IPv6 as defined in [RFC 4861] defines mechanisms for
//! solving the following problems:
//! - Router Discovery
//! - Prefix Discovery
//! - Parameter Discovery
//! - Address Autoconfiguration
//! - Address resolution
//! - Next-hop determination
//! - Neighbor Unreachability Detection
//! - Duplicate Address Detection
//! - Redirect
//!
//! [RFC 4861]: https://tools.ietf.org/html/rfc4861

use alloc::collections::{HashMap, HashSet};
use core::fmt::Debug;
use core::marker::PhantomData;
use core::num::NonZeroU8;
use core::time::Duration;

use assert_matches::assert_matches;
use log::{debug, error, trace};
use net_types::ip::{AddrSubnet, Ip, IpAddress, Ipv6, Ipv6Addr, Ipv6Scope, Ipv6SourceAddr, Subnet};
use net_types::{
    LinkLocalAddress, LinkLocalUnicastAddr, MulticastAddr, MulticastAddress, ScopeableAddress,
    SpecifiedAddr, SpecifiedAddress, UnicastAddr, Witness,
};
use nonzero_ext::nonzero;
use packet::{EmptyBuf, InnerPacketBuilder, Serializer};
use packet_formats::icmp::ndp::options::{
    NdpOption, NdpOptionBuilder, PrefixInformation, INFINITE_LIFETIME,
};
use packet_formats::icmp::ndp::{
    self, NeighborAdvertisement, NeighborSolicitation, Options, RouterSolicitation,
};
use packet_formats::icmp::{
    ndp::NdpPacket, IcmpMessage, IcmpPacket, IcmpPacketBuilder, IcmpUnusedCode,
};
use packet_formats::ip::Ipv6Proto;
use packet_formats::ipv6::Ipv6PacketBuilder;
use rand::{thread_rng, Rng};
use zerocopy::ByteSlice;

use crate::{
    context::{CounterContext, RngContext, StateContext, TimerContext},
    device::{
        link::{LinkAddress, LinkDevice},
        DeviceIdContext,
    },
    ip::device::state::{
        AddrConfig, AddrConfigType, AddressError, AddressState, IpDeviceState, SlaacConfig,
    },
    Instant,
};

const ZERO_DURATION: Duration = Duration::from_secs(0);

/// The IP packet hop limit for all NDP packets.
///
/// See [RFC 4861 section 4.1], [RFC 4861 section 4.2], [RFC 4861 section 4.2],
/// [RFC 4861 section 4.3], [RFC 4861 section 4.4], and [RFC 4861 section 4.5]
/// for more information.
///
/// [RFC 4861 section 4.1]: https://tools.ietf.org/html/rfc4861#section-4.1
/// [RFC 4861 section 4.2]: https://tools.ietf.org/html/rfc4861#section-4.2
/// [RFC 4861 section 4.3]: https://tools.ietf.org/html/rfc4861#section-4.3
/// [RFC 4861 section 4.4]: https://tools.ietf.org/html/rfc4861#section-4.4
/// [RFC 4861 section 4.5]: https://tools.ietf.org/html/rfc4861#section-4.5
const REQUIRED_NDP_IP_PACKET_HOP_LIMIT: u8 = 255;

/// The number of NS messages to be sent to perform DAD [RFC 4862 section 5.1].
///
/// [RFC 4862 section 5.1]: https://tools.ietf.org/html/rfc4862#section-5.1
pub(crate) const DUP_ADDR_DETECT_TRANSMITS: u8 = 1;

/// Minimum Valid Lifetime value to actually update an address's valid lifetime.
///
/// 2 hours.
const MIN_PREFIX_VALID_LIFETIME_FOR_UPDATE: Duration = Duration::from_secs(7200);

/// Required prefix length for SLAAC.
///
/// We need 64 bits in the prefix because the interface identifier is 64 bits,
/// and IPv6 addresses are 128 bits.
const REQUIRED_PREFIX_BITS: u8 = 64;

// Node Constants

/// The default value for the default hop limit to be used when sending IP
/// packets.
pub(crate) const HOP_LIMIT_DEFAULT: NonZeroU8 = nonzero!(64u8);

/// The default value for *BaseReachableTime* as defined in [RFC 4861 section
/// 10].
///
/// [RFC 4861 section 10]: https://tools.ietf.org/html/rfc4861#section-10
const REACHABLE_TIME_DEFAULT: Duration = Duration::from_secs(30);

/// The default value for *RetransTimer* as defined in [RFC 4861 section 10].
///
/// [RFC 4861 section 10]: https://tools.ietf.org/html/rfc4861#section-10
const RETRANS_TIMER_DEFAULT: Duration = Duration::from_secs(1);

/// The maximum number of multicast solicitations as defined in [RFC 4861
/// section 10].
///
/// [RFC 4861 section 10]: https://tools.ietf.org/html/rfc4861#section-10
const MAX_MULTICAST_SOLICIT: u8 = 3;

// Host Constants

/// Maximum number of Router Solicitation messages that may be sent when
/// attempting to discover routers. Each message sent must be separated by at
/// least `RTR_SOLICITATION_INTERVAL` as defined in [RFC 4861 section 10].
///
/// [RFC 4861 section 10]: https://tools.ietf.org/html/rfc4861#section-10
pub(crate) const MAX_RTR_SOLICITATIONS: u8 = 3;

/// Minimum duration between router solicitation messages as defined in [RFC
/// 4861 section 10].
///
/// [RFC 4861 section 10]: https://tools.ietf.org/html/rfc4861#section-10
const RTR_SOLICITATION_INTERVAL: Duration = Duration::from_secs(4);

/// Amount of time to wait after sending `MAX_RTR_SOLICITATIONS` Router
/// Solicitation messages before determining that there are no routers on the
/// link for the purpose of IPv6 Stateless Address Autoconfiguration if no
/// Router Advertisement messages have been received as defined in [RFC 4861
/// section 10].
///
/// This parameter is also used when a host sends its initial Router
/// Solicitation message, as per [RFC 4861 section 6.3.7]. Before a node sends
/// an initial solicitation, it SHOULD delay the transmission for a random
/// amount of time between 0 and `MAX_RTR_SOLICITATION_DELAY`. This serves to
/// alleviate congestion when many hosts start up on a link at the same time.
///
/// [RFC 4861 section 10]: https://tools.ietf.org/html/rfc4861#section-10
/// [RFC 4861 section 6.3.7]: https://tools.ietf.org/html/rfc4861#section-6.3.7
const MAX_RTR_SOLICITATION_DELAY: Duration = Duration::from_secs(1);

// NOTE(joshlf): The `LinkDevice` parameter may seem unnecessary. We only ever
// use the associated `Address` type, so why not just take that directly? By the
// same token, why have it as a parameter on `NdpState` and `NdpTimerId`? The
// answer is that, if we did, there would be no way to distinguish between
// different link device protocols that all happened to use the same hardware
// addressing scheme.
//
// Consider that the way that we implement context traits is via blanket impls.
// Even though each module's code _feels_ isolated from the rest of the system,
// in reality, all context impls end up on the same context type. In particular,
// all impls are of the form `impl<C: SomeContextTrait> SomeOtherContextTrait
// for C`. The `C` is the same throughout the whole stack.
//
// Thus, for two different link device protocols with the same hardware address
// type, if we used an `LinkAddress` parameter rather than a `LinkDevice`
// parameter, the `NdpContext` impls would conflict (in fact, the `StateContext`
// and `TimerContext` impls would conflict for similar reasons).

/// An NDP handler for NDP Events.
///
/// `NdpHandler<D>` is implemented for any type which implements
/// [`NdpContext<D>`], and it can also be mocked for use in testing.
pub(crate) trait NdpHandler<D: LinkDevice>: DeviceIdContext<D> {
    /// Cleans up state associated with the device.
    ///
    /// The contract is that after `deinitialize` is called, nothing else should
    /// be done with the state.
    fn deinitialize(&mut self, device_id: Self::DeviceId);

    /// Returns the configured retransmit timer.
    fn retrans_timer(&self, device_id: Self::DeviceId) -> Duration;

    /// Updates the NDP configuration for a `device_id`.
    ///
    /// Note, some values may not take effect immediately, and may only take
    /// effect the next time they are used. These scenarios documented below:
    ///
    ///  - Updates to [`NdpConfiguration::dup_addr_detect_transmits`] will only
    ///    take effect the next time Duplicate Address Detection (DAD) is done.
    ///    Any DAD processes that have already started will continue using the
    ///    old value.
    ///
    ///  - Updates to [`NdpConfiguration::max_router_solicitations`] will only
    ///    take effect the next time routers are explicitly solicited. Current
    ///    router solicitation will continue using the old value.
    fn set_configuration(&mut self, device_id: Self::DeviceId, config: NdpConfiguration);

    /// Start soliciting routers.
    ///
    /// Does nothing if a device's MAX_RTR_SOLICITATIONS parameters is `0`.
    ///
    /// # Panics
    ///
    /// Panics if we attempt to start router solicitation as a router, or if the
    /// device is already soliciting routers.
    fn start_soliciting_routers(&mut self, device_id: Self::DeviceId);

    /// Stop soliciting routers.
    ///
    /// Does nothing if the device is not soliciting routers.
    ///
    /// # Panics
    ///
    /// Panics if we attempt to stop router solicitations on a router (this
    /// should never happen as routers should not be soliciting other routers).
    fn stop_soliciting_routers(&mut self, device_id: Self::DeviceId);

    /// Look up the link layer address.
    ///
    /// Begins the address resolution process if the link layer address for
    /// `lookup_addr` is not already known.
    fn lookup(
        &mut self,
        device_id: Self::DeviceId,
        lookup_addr: UnicastAddr<Ipv6Addr>,
    ) -> Option<D::Address>;

    /// Handles a timer firing.
    fn handle_timer(&mut self, id: NdpTimerId<D, Self::DeviceId>);

    /// Insert a neighbor to the known neighbors table.
    ///
    /// This method only gets called when testing to force a neighbor so link
    /// address lookups completes immediately without doing address resolution.
    #[cfg(test)]
    fn insert_static_neighbor(
        &mut self,
        device_id: Self::DeviceId,
        net: UnicastAddr<Ipv6Addr>,
        hw: D::Address,
    );
}

impl<D: LinkDevice, C: NdpContext<D>> NdpHandler<D> for C
where
    D::Address: for<'a> From<&'a MulticastAddr<Ipv6Addr>>,
{
    fn deinitialize(&mut self, device_id: Self::DeviceId) {
        deinitialize(self, device_id)
    }

    fn retrans_timer(&self, device_id: Self::DeviceId) -> Duration {
        self.get_state_with(device_id).retrans_timer
    }

    fn set_configuration(&mut self, device_id: Self::DeviceId, config: NdpConfiguration) {
        set_ndp_configuration(self, device_id, config)
    }

    fn start_soliciting_routers(&mut self, device_id: Self::DeviceId) {
        start_soliciting_routers(self, device_id)
    }
    fn stop_soliciting_routers(&mut self, device_id: Self::DeviceId) {
        stop_soliciting_routers(self, device_id)
    }

    fn lookup(
        &mut self,
        device_id: Self::DeviceId,
        lookup_addr: UnicastAddr<Ipv6Addr>,
    ) -> Option<D::Address> {
        lookup(self, device_id, lookup_addr)
    }

    fn handle_timer(&mut self, id: NdpTimerId<D, Self::DeviceId>) {
        handle_timer(self, id)
    }

    #[cfg(test)]
    fn insert_static_neighbor(
        &mut self,
        device_id: Self::DeviceId,
        net: UnicastAddr<Ipv6Addr>,
        hw: D::Address,
    ) {
        insert_neighbor(self, device_id, net, hw)
    }
}

/// The execution context for an NDP device.
pub(crate) trait NdpContext<D: LinkDevice>:
    Sized
    + DeviceIdContext<D>
    + RngContext
    + CounterContext
    + StateContext<NdpState<D>, <Self as DeviceIdContext<D>>::DeviceId>
    + TimerContext<NdpTimerId<D, <Self as DeviceIdContext<D>>::DeviceId>>
{
    /// Get the link layer address for a device.
    fn get_link_layer_addr(&self, device_id: Self::DeviceId) -> UnicastAddr<D::Address>;

    /// Get the interface identifier for a device as defined by RFC 4291 section 2.5.1.
    fn get_interface_identifier(&self, device_id: Self::DeviceId) -> [u8; 8];

    /// Gets the IP state for this device.
    fn get_ip_device_state(&self, device_id: Self::DeviceId)
        -> &IpDeviceState<Self::Instant, Ipv6>;

    /// Gets the IP state for this device mutably.
    fn get_ip_device_state_mut(
        &mut self,
        device_id: Self::DeviceId,
    ) -> &mut IpDeviceState<Self::Instant, Ipv6>;

    /// Gets a non-tentative global or link-local address.
    ///
    /// Returns a non-tentative global address, if it is available. Otherwise,
    /// returns a link-local address, if it is available. Otherwise, returns
    /// `None`.
    fn get_non_tentative_global_or_link_local_addr(
        &self,
        device_id: Self::DeviceId,
    ) -> Option<UnicastAddr<Ipv6Addr>> {
        let mut non_tentative_addrs = self
            .get_ip_device_state(device_id)
            .iter_addrs()
            .filter(|entry| match entry.state {
                AddressState::Assigned | AddressState::Deprecated => true,
                AddressState::Tentative { dad_transmits_remaining: _ } => false,
            })
            .map(|entry| entry.addr_sub().addr());

        non_tentative_addrs
            .clone()
            .find(|addr| addr.scope() == Ipv6Scope::Global)
            .or_else(|| non_tentative_addrs.find(|addr| addr.is_link_local()))
    }

    // TODO(joshlf): Use `FrameContext` instead.

    /// Send a packet in a device layer frame.
    ///
    /// `send_ipv6_frame` accepts a device ID, a next hop IP address, and a
    /// `Serializer`. Implementers must resolve the destination link-layer
    /// address from the provided `next_hop` IPv6 address.
    ///
    /// # Panics
    ///
    /// May panic if `device_id` is not initialized. See
    /// [`crate::device::initialize_device`] for more information.
    fn send_ipv6_frame<S: Serializer<Buffer = EmptyBuf>>(
        &mut self,
        device_id: Self::DeviceId,
        next_hop: SpecifiedAddr<Ipv6Addr>,
        body: S,
    ) -> Result<(), S>;

    /// Notifies device layer that the link-layer address for the neighbor in
    /// `address` has been resolved to `link_address`.
    ///
    /// Implementers may use this signal to dispatch any packets that were
    /// queued waiting for address resolution.
    fn address_resolved(
        &mut self,
        device_id: Self::DeviceId,
        address: &UnicastAddr<Ipv6Addr>,
        link_address: D::Address,
    );

    /// Notifies the device layer that the link-layer address resolution for the
    /// neighbor in `address` failed.
    fn address_resolution_failed(
        &mut self,
        device_id: Self::DeviceId,
        address: &UnicastAddr<Ipv6Addr>,
    );

    /// Notifies the device layer that a duplicate address has been detected.
    /// The device should want to remove the address.
    fn duplicate_address_detected(
        &mut self,
        device_id: Self::DeviceId,
        addr: UnicastAddr<Ipv6Addr>,
    );

    /// Set Link MTU.
    ///
    /// `set_mtu` is used when a host receives a Router Advertisement with the
    /// MTU option.
    ///
    /// `set_mtu` MAY set the device's new MTU to a value less than `mtu` if the
    /// device does not support using `mtu` as its new MTU. `set_mtu` MUST NOT
    /// use a new MTU value that is greater than `mtu`.
    ///
    /// See [RFC 4861 section 6.3.4] for more information.
    ///
    /// # Panics
    ///
    /// `set_mtu` is allowed to panic if `mtu` is less than the IPv6 minimum
    /// link MTU, [`Ipv6::MINIMUM_LINK_MTU`].
    ///
    /// [RFC 4861 section 6.3.4]: https://tools.ietf.org/html/rfc4861#section-6.3.4
    fn set_mtu(&mut self, device_id: Self::DeviceId, mtu: u32);

    /// Add a new IPv6 Global Address configured via SLAAC.
    fn add_slaac_addr_sub(
        &mut self,
        device_id: Self::DeviceId,
        addr_sub: AddrSubnet<Ipv6Addr, UnicastAddr<Ipv6Addr>>,
        valid_until: Self::Instant,
    ) -> Result<(), AddressError>;

    /// Deprecate the use of an address previously configured via SLAAC.
    ///
    /// If `addr` is currently tentative on `device_id`, the address should
    /// simply be invalidated as new connections should not use a deprecated
    /// address, and we should have no existing connections using a tentative
    /// address.
    ///
    /// # Panics
    ///
    /// May panic if `addr` is not an address configured via SLAAC on
    /// `device_id`.
    fn deprecate_slaac_addr(&mut self, device_id: Self::DeviceId, addr: &UnicastAddr<Ipv6Addr>);

    /// Completely invalidate an address previously configured via SLAAC.
    ///
    /// # Panics
    ///
    /// May panic if `addr` is not an address configured via SLAAC on
    /// `device_id`.
    fn invalidate_slaac_addr(&mut self, device_id: Self::DeviceId, addr: &UnicastAddr<Ipv6Addr>);

    /// Update the instant when an address configured via SLAAC is valid until.
    ///
    /// # Panics
    ///
    /// May panic if `addr` is not an address configured via SLAAC on
    /// `device_id`.
    fn update_slaac_addr_valid_until(
        &mut self,
        device_id: Self::DeviceId,
        addr: &UnicastAddr<Ipv6Addr>,
        valid_until: Self::Instant,
    ) {
        trace!(
            "NdpContext::update_slaac_addr_valid_until: updating address {:?}'s valid until instant to {:?} on device {:?}",
            addr,
            valid_until,
            device_id
        );

        let slaac_config = self
            .get_ip_device_state_mut(device_id)
            .iter_global_ipv6_addrs_mut()
            .find_map(|a| {
                if a.addr_sub().addr() == *addr {
                    match &mut a.config {
                        AddrConfig::Slaac(slaac) => Some(slaac),
                        AddrConfig::Manual => None,
                    }
                } else {
                    None
                }
            })
            .expect("address is not configured via SLAAC on this device");
        match slaac_config {
            SlaacConfig { valid_until: v } => *v = Some(valid_until),
        };
    }

    /// Is the netstack currently operating as an IPv6 router?
    ///
    /// Returns `true` if the netstack is configured to route IPv6 packets not
    /// destined for it. Note that this does not necessarily mean that routing
    /// is enabled on any given interface. That is configured separately on a
    /// per-interface basis.
    fn is_router(&self) -> bool;

    /// Can `device_id` route IP packets not destined for it?
    ///
    /// If `is_router` returns `true`, we know that both the `device_id` and the
    /// netstack (`ctx`) have routing enabled; if `is_router` returns false,
    /// either `device_id` or the netstack (`ctx`) has routing disabled.
    fn is_router_device(&self, device_id: Self::DeviceId) -> bool {
        self.is_router() && self.get_ip_device_state(device_id).routing_enabled
    }
}

fn deinitialize<D: LinkDevice, C: NdpContext<D>>(ctx: &mut C, device_id: C::DeviceId) {
    // Remove all timers associated with the device
    ctx.cancel_timers_with(|timer_id| timer_id.get_device_id() == device_id);
    // TODO(rheacock): Send any immediate packets, and potentially flag the
    // state as uninitialized?
}

/// Per interface configuration for NDP.
#[derive(Debug, Clone)]
pub struct NdpConfiguration {
    /// Value for NDP's MAX_RTR_SOLICITATIONS parameter to configure how many
    /// router solicitation messages to send on interface enable.
    ///
    /// As per [RFC 4861 section 6.3.7], a host SHOULD transmit up to
    /// `MAX_RTR_SOLICITATIONS` Router Solicitation messages. Given the RFC does
    /// not require us to send `MAX_RTR_SOLICITATIONS` messages, we allow a
    /// configurable value, up to `MAX_RTR_SOLICITATIONS`.
    ///
    /// Default: [`MAX_RTR_SOLICITATIONS`].
    max_router_solicitations: Option<NonZeroU8>,
}

impl Default for NdpConfiguration {
    fn default() -> Self {
        Self { max_router_solicitations: NonZeroU8::new(MAX_RTR_SOLICITATIONS) }
    }
}

impl NdpConfiguration {
    /// Get the value for NDP's MAX_RTR_SOLICITATIONS parameter.
    pub fn get_max_router_solicitations(&self) -> Option<NonZeroU8> {
        self.max_router_solicitations
    }

    /// Set the value for NDP's MAX_RTR_SOLICITATIONS parameter.
    ///
    /// A value of `None` means no router solicitations will be sent.
    /// `MAX_RTR_SOLICITATIONS` is the maximum possible value; values will be
    /// saturated at `MAX_RTR_SOLICITATIONS`.
    pub fn set_max_router_solicitations(&mut self, mut v: Option<NonZeroU8>) {
        if let Some(inner) = v {
            if inner.get() > MAX_RTR_SOLICITATIONS {
                v = NonZeroU8::new(MAX_RTR_SOLICITATIONS);
            }
        }

        self.max_router_solicitations = v;
    }
}

/// The state associated with an instance of the Neighbor Discovery Protocol
/// (NDP).
///
/// Each device will contain an `NdpState` object to keep track of discovery
/// operations.
pub(crate) struct NdpState<D: LinkDevice> {
    //
    // NDP operation data structures.
    //
    /// List of neighbors.
    neighbors: NeighborTable<D::Address>,

    /// List of default routers, indexed by their link-local address.
    default_routers: HashSet<LinkLocalUnicastAddr<Ipv6Addr>>,

    /// List of on-link prefixes.
    on_link_prefixes: HashSet<Subnet<Ipv6Addr>>,

    /// Number of remaining Router Solicitation messages to send.
    router_solicitations_remaining: u8,

    //
    // Interface parameters learned from Router Advertisements.
    //
    // See RFC 4861 section 6.3.2.
    //
    /// A base value used for computing the random `reachable_time` value.
    ///
    /// Default: `REACHABLE_TIME_DEFAULT`.
    ///
    /// See BaseReachableTime in [RFC 4861 section 6.3.2] for more details.
    ///
    /// [RFC 4861 section 6.3.2]: https://tools.ietf.org/html/rfc4861#section-6.3.2
    base_reachable_time: Duration,

    /// The time a neighbor is considered reachable after receiving a
    /// reachability confirmation.
    ///
    /// This value should be uniformly distributed between MIN_RANDOM_FACTOR
    /// (0.5) and MAX_RANDOM_FACTOR (1.5) times `base_reachable_time`
    /// milliseconds. A new random should be calculated when
    /// `base_reachable_time` changes (due to Router Advertisements) or at least
    /// every few hours even if no Router Advertisements are received.
    ///
    /// See ReachableTime in [RFC 4861 section 6.3.2] for more details.
    ///
    /// [RFC 4861 section 6.3.2]: https://tools.ietf.org/html/rfc4861#section-6.3.2
    // TODO(fxbug.dev/69490): Remove this or explain why it's here.
    #[allow(dead_code)]
    reachable_time: Duration,

    /// The time between retransmissions of Neighbor Solicitation messages to a
    /// neighbor when resolving the address or when probing the reachability of
    /// a neighbor.
    ///
    /// Default: `RETRANS_TIMER_DEFAULT`.
    ///
    /// See RetransTimer in [RFC 4861 section 6.3.2] for more details.
    ///
    /// [RFC 4861 section 6.3.2]: https://tools.ietf.org/html/rfc4861#section-6.3.2
    retrans_timer: Duration,

    /// NDP configuration.
    config: NdpConfiguration,
}

impl<D: LinkDevice> NdpState<D> {
    pub(crate) fn new(config: NdpConfiguration) -> Self {
        let mut ret = Self {
            neighbors: NeighborTable::default(),
            default_routers: HashSet::new(),
            on_link_prefixes: HashSet::new(),
            router_solicitations_remaining: 0,

            base_reachable_time: REACHABLE_TIME_DEFAULT,
            reachable_time: REACHABLE_TIME_DEFAULT,
            retrans_timer: RETRANS_TIMER_DEFAULT,
            config,
        };

        // Calculate an actually random `reachable_time` value instead of using
        // a constant.
        ret.recalculate_reachable_time();

        ret
    }

    // NDP operation data structure helpers.

    /// Do we know about the default router identified by `ip`?
    fn has_default_router(&self, ip: &LinkLocalUnicastAddr<Ipv6Addr>) -> bool {
        self.default_routers.contains(&ip)
    }

    /// Adds a new router to our list of default routers.
    fn add_default_router(&mut self, ip: LinkLocalUnicastAddr<Ipv6Addr>) {
        // Router must not already exist if we are adding it.
        assert!(self.default_routers.insert(ip));
    }

    /// Removes a router from our list of default routers.
    fn remove_default_router(&mut self, ip: &LinkLocalUnicastAddr<Ipv6Addr>) {
        // Router must exist if we are removing it.
        assert!(self.default_routers.remove(&ip));
    }

    /// Handle the invalidation of a default router.
    ///
    /// # Panics
    ///
    /// Panics if the router has not yet been discovered.
    fn invalidate_default_router(&mut self, ip: &LinkLocalUnicastAddr<Ipv6Addr>) {
        // As per RFC 4861 section 6.3.5:
        // Whenever the Lifetime of an entry in the Default Router List expires,
        // that entry is discarded.  When removing a router from the Default
        // Router list, the node MUST update the Destination Cache in such a way
        // that all entries using the router perform next-hop determination
        // again rather than continue sending traffic to the (deleted) router.

        self.remove_default_router(ip);

        // If a neighbor entry exists for the router, unmark it as a router.
        if let Some(state) = self.neighbors.get_neighbor_state_mut(&ip) {
            state.is_router = false;
        }
    }

    /// Do we already know about this prefix?
    fn has_prefix(&self, subnet: &Subnet<Ipv6Addr>) -> bool {
        self.on_link_prefixes.contains(subnet)
    }

    /// Adds a new prefix to our list of on-link prefixes.
    ///
    /// # Panics
    ///
    /// Panics if the prefix already exists in our list of on-link prefixes.
    fn add_prefix(&mut self, subnet: Subnet<Ipv6Addr>) {
        assert!(self.on_link_prefixes.insert(subnet));
    }

    /// Removes a prefix from our list of on-link prefixes.
    ///
    /// # Panics
    ///
    /// Panics if the prefix doesn't exist in our list of on-link prefixes.
    fn remove_prefix(&mut self, subnet: &Subnet<Ipv6Addr>) {
        assert!(self.on_link_prefixes.remove(subnet));
    }

    /// Handle the invalidation of a prefix.
    ///
    /// # Panics
    ///
    /// Panics if the prefix doesn't exist in our list of on-link prefixes.
    fn invalidate_prefix(&mut self, subnet: Subnet<Ipv6Addr>) {
        // As per RFC 4861 section 6.3.5:
        // Whenever the invalidation timer expires for a Prefix List entry, that
        // entry is discarded. No existing Destination Cache entries need be
        // updated, however. Should a reachability problem arise with an
        // existing Neighbor Cache entry, Neighbor Unreachability Detection will
        // perform any needed recovery.

        self.remove_prefix(&subnet);
    }

    // Interface parameters learned from Router Advertisements.

    /// Set the base value used for computing the random `reachable_time` value.
    ///
    /// This method will also recalculate the `reachable_time` if the new base
    /// value is different from the current value. If the new base value is the
    /// same as the current value, `set_base_reachable_time` does nothing.
    pub(crate) fn set_base_reachable_time(&mut self, v: Duration) {
        assert_ne!(Duration::new(0, 0), v);

        if self.base_reachable_time == v {
            return;
        }

        self.base_reachable_time = v;

        self.recalculate_reachable_time();
    }

    /// Recalculate `reachable_time`.
    ///
    /// The new `reachable_time` will be a random value between a factor of
    /// MIN_RANDOM_FACTOR and MAX_RANDOM_FACTOR, as per [RFC 4861 section
    /// 6.3.2].
    ///
    /// [RFC 4861 section 6.3.2]: https://tools.ietf.org/html/rfc4861#section-6.3.2
    pub(crate) fn recalculate_reachable_time(&mut self) {
        let base = self.base_reachable_time;
        let half = base / 2;
        let reachable_time = half + thread_rng().gen_range(Duration::new(0, 0)..base);

        // Random value must between a factor of MIN_RANDOM_FACTOR (0.5) and
        // MAX_RANDOM_FACTOR (1.5), as per RFC 4861 section 6.3.2.
        assert!((reachable_time >= half) && (reachable_time <= (base + half)));

        self.reachable_time = reachable_time;
    }

    /// Set the time between retransmissions of Neighbor Solicitation messages
    /// to a neighbor when resolving the address or when probing the
    /// reachability of a neighbor.
    pub(crate) fn set_retrans_timer(&mut self, v: Duration) {
        assert_ne!(Duration::new(0, 0), v);

        self.retrans_timer = v;
    }
}

/// The identifier for timer events in NDP operations.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Hash)]
pub(crate) struct NdpTimerId<D: LinkDevice, DeviceId> {
    device_id: DeviceId,
    inner: InnerNdpTimerId,
    _marker: PhantomData<D>,
}

/// The types of NDP timers.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Hash)]
pub(crate) enum InnerNdpTimerId {
    /// This is used to retry sending Neighbor Discovery Protocol requests.
    LinkAddressResolution { neighbor_addr: UnicastAddr<Ipv6Addr> },
    /// Timer to send Router Solicitation messages.
    RouterSolicitationTransmit,
    /// Timer to invalidate a router.
    ///
    /// `ip` is the identifying IP of the router.
    RouterInvalidation { ip: LinkLocalUnicastAddr<Ipv6Addr> },
    /// Timer to invalidate a prefix.
    PrefixInvalidation { subnet: Subnet<Ipv6Addr> },
    /// Timer to deprecate an address configured via SLAAC.
    DeprecateSlaacAddress { addr: UnicastAddr<Ipv6Addr> },
    /// Timer to invalidate an address configured via SLAAC.
    InvalidateSlaacAddress { addr: UnicastAddr<Ipv6Addr> },
    // TODO: The RFC suggests that we SHOULD make a random delay to join the
    // solicitation group. When we support MLD, we probably want one for that.
}

impl<D: LinkDevice, DeviceId: Copy> NdpTimerId<D, DeviceId> {
    fn new(device_id: DeviceId, inner: InnerNdpTimerId) -> NdpTimerId<D, DeviceId> {
        NdpTimerId { device_id, inner, _marker: PhantomData }
    }

    /// Creates a new `NdpTimerId` wrapped inside a `TimerId` with the provided
    /// `device_id` and `neighbor_addr`.
    pub(crate) fn new_link_address_resolution(
        device_id: DeviceId,
        neighbor_addr: UnicastAddr<Ipv6Addr>,
    ) -> NdpTimerId<D, DeviceId> {
        NdpTimerId::new(device_id, InnerNdpTimerId::LinkAddressResolution { neighbor_addr })
    }

    pub(crate) fn new_router_solicitation(device_id: DeviceId) -> NdpTimerId<D, DeviceId> {
        NdpTimerId::new(device_id, InnerNdpTimerId::RouterSolicitationTransmit)
    }

    pub(crate) fn new_router_invalidation(
        device_id: DeviceId,
        ip: LinkLocalUnicastAddr<Ipv6Addr>,
    ) -> NdpTimerId<D, DeviceId> {
        NdpTimerId::new(device_id, InnerNdpTimerId::RouterInvalidation { ip })
    }

    pub(crate) fn new_prefix_invalidation(
        device_id: DeviceId,
        subnet: Subnet<Ipv6Addr>,
    ) -> NdpTimerId<D, DeviceId> {
        NdpTimerId::new(device_id, InnerNdpTimerId::PrefixInvalidation { subnet })
    }

    pub(crate) fn new_deprecate_slaac_address(
        device_id: DeviceId,
        addr: UnicastAddr<Ipv6Addr>,
    ) -> NdpTimerId<D, DeviceId> {
        NdpTimerId::new(device_id, InnerNdpTimerId::DeprecateSlaacAddress { addr })
    }

    pub(crate) fn new_invalidate_slaac_address(
        device_id: DeviceId,
        addr: UnicastAddr<Ipv6Addr>,
    ) -> NdpTimerId<D, DeviceId> {
        NdpTimerId::new(device_id, InnerNdpTimerId::InvalidateSlaacAddress { addr })
    }

    pub(crate) fn get_device_id(&self) -> DeviceId {
        self.device_id
    }
}

fn handle_timer<D: LinkDevice, C: NdpContext<D>>(ctx: &mut C, id: NdpTimerId<D, C::DeviceId>) {
    match id.inner {
        InnerNdpTimerId::LinkAddressResolution { neighbor_addr } => {
            let ndp_state = ctx.get_state_mut_with(id.device_id);
            if let Some(NeighborState {
                state: NeighborEntryState::Incomplete { transmit_counter },
                ..
            }) = ndp_state.neighbors.get_neighbor_state_mut(&neighbor_addr)
            {
                if *transmit_counter < MAX_MULTICAST_SOLICIT {
                    let retrans_timer = ndp_state.retrans_timer;

                    // Increase the transmit counter and send the solicitation
                    // again
                    *transmit_counter += 1;
                    send_neighbor_solicitation(ctx, id.device_id, neighbor_addr);
                    let _: Option<C::Instant> = ctx.schedule_timer(
                        retrans_timer,
                        NdpTimerId::new_link_address_resolution(id.device_id, neighbor_addr).into(),
                    );
                } else {
                    // To make sure we don't get stuck in this neighbor
                    // unreachable state forever, remove the neighbor from the
                    // database:
                    ndp_state.neighbors.delete_neighbor_state(&neighbor_addr);
                    ctx.increment_counter("ndp::neighbor_solicitation_timer");

                    ctx.address_resolution_failed(id.device_id, &neighbor_addr);
                }
            } else {
                unreachable!("handle_timer: timer for neighbor {:?} address resolution should not exist if no entry exists", neighbor_addr);
            }
        }
        InnerNdpTimerId::RouterSolicitationTransmit => do_router_solicitation(ctx, id.device_id),
        InnerNdpTimerId::RouterInvalidation { ip } => {
            // Invalidate the router.
            //
            // The call to `invalidate_default_router` may panic if `ip` does
            // not reference a known default router, but we will only reach here
            // if we received an NDP Router Advertisement from a router with a
            // valid lifetime > 0, at which point this timeout. would have been
            // set. Given this, we know that `invalidate_default_router` will
            // not panic.
            ctx.get_state_mut_with(id.device_id).invalidate_default_router(&ip)
        }
        InnerNdpTimerId::PrefixInvalidation { subnet } => {
            // Invalidate the prefix.
            //
            // The call to `invalidate_prefix` may panic if `addr_subnet` is not
            // in the list of on-link prefixes. However, we will only reach here
            // if we received an NDP Router Advertisement with the prefix option
            // with the on-link flag set. Given this we know that `addr_subnet`
            // must exist if this timer was fired so `invalidate_prefix` will
            // not panic.
            ctx.get_state_mut_with(id.device_id).invalidate_prefix(subnet);
        }
        InnerNdpTimerId::DeprecateSlaacAddress { addr } => {
            ctx.deprecate_slaac_addr(id.device_id, &addr);
        }
        InnerNdpTimerId::InvalidateSlaacAddress { addr } => {
            ctx.invalidate_slaac_addr(id.device_id, &addr);
        }
    }
}

fn set_ndp_configuration<D: LinkDevice, C: NdpContext<D>>(
    ctx: &mut C,
    device_id: C::DeviceId,
    config: NdpConfiguration,
) {
    ctx.get_state_mut_with(device_id).config = config;
}

fn lookup<D: LinkDevice, C: NdpContext<D>>(
    ctx: &mut C,
    device_id: C::DeviceId,
    lookup_addr: UnicastAddr<Ipv6Addr>,
) -> Option<D::Address>
where
    D::Address: for<'a> From<&'a MulticastAddr<Ipv6Addr>>,
{
    trace!("ndp::lookup: {:?}", lookup_addr);

    // TODO(brunodalbo): Figure out what to do if a frame can't be sent
    let ndpstate = ctx.get_state_mut_with(device_id);
    let result = ndpstate.neighbors.get_neighbor_state(&lookup_addr);

    match result {
        // TODO(ghanan): As long as have ever received a link layer address for
        //               `lookup_addr` from any NDP packet with the source link
        //               layer option, we would have stored that address. Here
        //               we simply return that address without checking the
        //               actual state of the neighbor entry. We should make sure
        //               that the entry is not Stale before returning the
        //               address. If it is stale, we should make sure it is
        //               reachable first. See RFC 4861 section 7.3.2 for more
        //               information.
        Some(NeighborState { link_address: Some(address), .. }) => Some(*address),

        // We do not know about the neighbor and need to start address
        // resolution.
        None => {
            trace!("ndp::lookup: starting address resolution process for {:?}", lookup_addr);

            let retrans_timer = ndpstate.retrans_timer;

            // If we're not already waiting for a neighbor solicitation
            // response, mark it as Incomplete and send a neighbor solicitation,
            // also setting the transmission count to 1.
            ndpstate.neighbors.add_incomplete_neighbor_state(lookup_addr);

            send_neighbor_solicitation(ctx, device_id, lookup_addr);

            // Also schedule a timer to retransmit in case we don't get neighbor
            // advertisements back.
            let _: Option<C::Instant> = ctx.schedule_timer(
                retrans_timer,
                NdpTimerId::new_link_address_resolution(device_id, lookup_addr).into(),
            );

            // Returning `None` as we do not have a link-layer address to give
            // yet.
            None
        }

        // Address resolution is currently in progress.
        Some(NeighborState { state: NeighborEntryState::Incomplete { .. }, .. }) => {
            trace!(
                "ndp::lookup: still waiting for address resolution to complete for {:?}",
                lookup_addr
            );
            None
        }

        // TODO(ghanan): Handle case where a neighbor entry exists for a
        //               `link_addr` but no link address as been discovered.
        _ => unimplemented!("A neighbor entry exists but no link address is discovered"),
    }
}

#[cfg(test)]
fn insert_neighbor<D: LinkDevice, C: NdpContext<D>>(
    ctx: &mut C,
    device_id: C::DeviceId,
    net: UnicastAddr<Ipv6Addr>,
    hw: D::Address,
) {
    // Neighbor `net` should be marked as reachable.
    ctx.get_state_mut_with(device_id).neighbors.set_link_address(net, hw, true)
}

/// `NeighborState` keeps all state that NDP may want to keep about neighbors,
/// like link address resolution and reachability information, for example.
#[cfg_attr(test, derive(Debug, Eq, PartialEq))]
struct NeighborState<H> {
    is_router: bool,
    state: NeighborEntryState,
    link_address: Option<H>,
}

impl<H> NeighborState<H> {
    fn new() -> Self {
        Self {
            is_router: false,
            state: NeighborEntryState::Incomplete { transmit_counter: 0 },
            link_address: None,
        }
    }

    /// Is the neighbor incomplete (waiting for address resolution)?
    fn is_incomplete(&self) -> bool {
        if let NeighborEntryState::Incomplete { .. } = self.state {
            true
        } else {
            false
        }
    }

    /// Is the neighbor reachable?
    fn is_reachable(&self) -> bool {
        self.state == NeighborEntryState::Reachable
    }
}

/// The various states a Neighbor cache entry can be in.
///
/// See [RFC 4861 section 7.3.2].
///
/// [RFC 4861 section 7.3.2]: https://tools.ietf.org/html/rfc4861#section-7.3.2
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum NeighborEntryState {
    /// Address resolution is being performed on the entry. Specifically, a
    /// Neighbor Solicitation has been sent to the solicited-node multicast
    /// address of the target, but the corresponding Neighbor Advertisement has
    /// not yet been received.
    ///
    /// `transmit_counter` is the count of Neighbor Solicitation messages sent
    /// as part of the Address resolution process.
    Incomplete { transmit_counter: u8 },

    /// Positive confirmation was received within the last ReachableTime
    /// milliseconds that the forward path to the neighbor was functioning
    /// properly.  While `Reachable`, no special action takes place as packets
    /// are sent.
    Reachable,

    /// More than ReachableTime milliseconds have elapsed since the last
    /// positive confirmation was received that the forward path was functioning
    /// properly.  While stale, no action takes place until a packet is sent.
    ///
    /// The `Stale` state is entered upon receiving an unsolicited Neighbor
    /// Discovery message that updates the cached link-layer address.  Receipt
    /// of such a message does not confirm reachability, and entering the
    /// `Stale` state ensures reachability is verified quickly if the entry is
    /// actually being used.  However, reachability is not actually verified
    /// until the entry is actually used.
    Stale,

    /// More than ReachableTime milliseconds have elapsed since the last
    /// positive confirmation was received that the forward path was functioning
    /// properly, and a packet was sent within the last DELAY_FIRST_PROBE_TIME
    /// seconds.  If no reachability confirmation is received within
    /// DELAY_FIRST_PROBE_TIME seconds of entering the DELAY state, send a
    /// Neighbor Solicitation and change the state to PROBE.
    ///
    /// The DELAY state is an optimization that gives upper- layer protocols
    /// additional time to provide reachability confirmation in those cases
    /// where ReachableTime milliseconds have passed since the last confirmation
    /// due to lack of recent traffic.  Without this optimization, the opening
    /// of a TCP connection after a traffic lull would initiate probes even
    /// though the subsequent three-way handshake would provide a reachability
    /// confirmation almost immediately.
    _Delay,

    /// A reachability confirmation is actively sought by retransmitting
    /// Neighbor Solicitations every RetransTimer milliseconds until a
    /// reachability confirmation is received.
    _Probe,
}

struct NeighborTable<H> {
    table: HashMap<UnicastAddr<Ipv6Addr>, NeighborState<H>>,
}

impl<H: PartialEq + Debug> NeighborTable<H> {
    /// Sets the link address for a neighbor.
    ///
    /// If `is_reachable` is `true`, the state of the neighbor will be set to
    /// `NeighborEntryState::Reachable`. Otherwise, it will be set to
    /// `NeighborEntryState::Stale` if the address was updated. A `false` value
    /// for `is_reachable` does not mean that the neighbor is unreachable, it
    /// just means that we do not know if it is reachable.
    fn set_link_address(
        &mut self,
        neighbor: UnicastAddr<Ipv6Addr>,
        address: H,
        is_reachable: bool,
    ) {
        let address = Some(address);
        let neighbor_state = self.table.entry(neighbor).or_insert_with(NeighborState::new);

        trace!("set_link_address: setting link address for neighbor {:?} to address", address);

        if is_reachable {
            trace!("set_link_address: reachability is known, so setting state for neighbor {:?} to Reachable", neighbor);

            neighbor_state.state = NeighborEntryState::Reachable;
        } else if neighbor_state.link_address != address {
            trace!("set_link_address: new link addr different from old and reachability is unknown, so setting state for neighbor {:?} to Stale", neighbor);

            neighbor_state.state = NeighborEntryState::Stale;
        }

        neighbor_state.link_address = address;
    }
}

impl<H> NeighborTable<H> {
    /// Create a new incomplete state of a neighbor, setting the transmit
    /// counter to 1.
    fn add_incomplete_neighbor_state(&mut self, neighbor: UnicastAddr<Ipv6Addr>) {
        let mut state = NeighborState::new();
        state.state = NeighborEntryState::Incomplete { transmit_counter: 1 };

        let _: Option<_> = self.table.insert(neighbor, state);
    }

    /// Get the neighbor's state, if it exists.
    fn get_neighbor_state(&self, neighbor: &UnicastAddr<Ipv6Addr>) -> Option<&NeighborState<H>> {
        self.table.get(neighbor)
    }

    /// Get a  the neighbor's mutable state, if it exists.
    fn get_neighbor_state_mut(
        &mut self,
        neighbor: &UnicastAddr<Ipv6Addr>,
    ) -> Option<&mut NeighborState<H>> {
        self.table.get_mut(neighbor)
    }

    /// Delete the neighbor's state, if it exists.
    fn delete_neighbor_state(&mut self, neighbor: &UnicastAddr<Ipv6Addr>) {
        let _: Option<_> = self.table.remove(neighbor);
    }
}

impl<H> Default for NeighborTable<H> {
    fn default() -> Self {
        NeighborTable { table: HashMap::default() }
    }
}

fn start_soliciting_routers<D: LinkDevice, C: NdpContext<D>>(ctx: &mut C, device_id: C::DeviceId) {
    // MUST NOT be a router.
    assert!(!ctx.is_router_device(device_id));

    let ndp_state = ctx.get_state_mut_with(device_id);

    // MUST NOT already be performing router solicitation.
    assert_eq!(ndp_state.router_solicitations_remaining, 0);

    if let Some(v) = ndp_state.config.max_router_solicitations {
        trace!(
            "ndp::start_soliciting_routers: start soliciting routers for device: {:?}",
            device_id
        );

        ndp_state.router_solicitations_remaining = v.get();

        // As per RFC 4861 section 6.3.7, delay the first transmission for a
        // random amount of time between 0 and `MAX_RTR_SOLICITATION_DELAY` to
        // alleviate congestion when many hosts start up on a link at the same
        // time.
        let delay = ctx.rng_mut().gen_range(Duration::new(0, 0)..MAX_RTR_SOLICITATION_DELAY);

        // MUST NOT already be performing router solicitation.
        assert_eq!(
            ctx.schedule_timer(delay, NdpTimerId::new_router_solicitation(device_id).into()),
            None
        );
    } else {
        trace!("ndp::start_soliciting_routers: device {:?} not configured to send any router solicitations", device_id);
    }
}

fn stop_soliciting_routers<D: LinkDevice, C: NdpContext<D>>(ctx: &mut C, device_id: C::DeviceId) {
    trace!("ndp::stop_soliciting_routers: stop soliciting routers for device: {:?}", device_id);

    assert!(!ctx.is_router_device(device_id));

    let _: Option<C::Instant> =
        ctx.cancel_timer(NdpTimerId::new_router_solicitation(device_id).into());

    // No more router solicitations remaining since we are cancelling.
    ctx.get_state_mut_with(device_id).router_solicitations_remaining = 0;
}

/// Solicit routers once amd schedule next message.
///
/// # Panics
///
/// Panics if we attempt to do router solicitation as a router or if we are
/// already done soliciting routers.
fn do_router_solicitation<D: LinkDevice, C: NdpContext<D>>(ctx: &mut C, device_id: C::DeviceId) {
    assert!(!ctx.is_router_device(device_id));

    let ndp_state = ctx.get_state_mut_with(device_id);
    let remaining = &mut ndp_state.router_solicitations_remaining;

    assert!(*remaining > 0);
    *remaining -= 1;
    let remaining = *remaining;

    let src_ip = ctx.get_non_tentative_global_or_link_local_addr(device_id);

    trace!(
        "do_router_solicitation: soliciting routers for device {:?} using src_ip {:?}",
        device_id,
        src_ip
    );

    send_router_solicitation(ctx, device_id, src_ip);

    if remaining == 0 {
        trace!(
            "do_router_solicitation: done sending router solicitation messages for device {:?}",
            device_id
        );
        return;
    } else {
        // TODO(ghanan): Make the interval between messages configurable.
        let _: Option<C::Instant> = ctx.schedule_timer(
            RTR_SOLICITATION_INTERVAL,
            NdpTimerId::new_router_solicitation(device_id).into(),
        );
    }
}

/// Send a router solicitation packet.
///
/// # Panics
///
/// Panics if we attempt to send a router solicitation as a router.
fn send_router_solicitation<D: LinkDevice, C: NdpContext<D>>(
    ctx: &mut C,
    device_id: C::DeviceId,
    src_ip: Option<UnicastAddr<Ipv6Addr>>,
) {
    assert!(!ctx.get_ip_device_state(device_id).routing_enabled);

    trace!("send_router_solicitation: sending router solicitation from {:?}", src_ip);

    match src_ip {
        Some(src_ip) => {
            let src_ll = ctx.get_link_layer_addr(device_id);
            // TODO(https://fxbug.dev/85055): Either panic or guarantee that
            // this error can't happen statically?
            let _ = send_ndp_packet::<_, _, &[u8], _>(
                ctx,
                device_id,
                src_ip.get(),
                Ipv6::ALL_ROUTERS_LINK_LOCAL_MULTICAST_ADDRESS.into_specified(),
                RouterSolicitation::default(),
                &[NdpOptionBuilder::SourceLinkLayerAddress(src_ll.bytes())],
            );
        }
        None => {
            // Must not include the source link layer address if the source address
            // is unspecified as per RFC 4861 section 4.1.
            // TODO(https://fxbug.dev/85055): Either panic or guarantee that
            // this error can't happen statically?
            let _ = send_ndp_packet::<_, _, &[u8], _>(
                ctx,
                device_id,
                Ipv6::UNSPECIFIED_ADDRESS,
                Ipv6::ALL_ROUTERS_LINK_LOCAL_MULTICAST_ADDRESS.into_specified(),
                RouterSolicitation::default(),
                &[],
            );
        }
    }
}

fn send_neighbor_solicitation<D: LinkDevice, C: NdpContext<D>>(
    ctx: &mut C,
    device_id: C::DeviceId,
    lookup_addr: UnicastAddr<Ipv6Addr>,
) {
    trace!("send_neighbor_solicitation: lookup_addr {:?}", lookup_addr);

    // TODO(brunodalbo) when we send neighbor solicitations, we SHOULD set the
    //  source IP to the same IP as the packet that triggered the solicitation,
    //  so that when we hit the neighbor they'll have us in their cache,
    //  reducing overall burden on the network.
    if let Some(src_ip) = ctx.get_non_tentative_global_or_link_local_addr(device_id) {
        assert!(src_ip.is_valid_unicast());
        let src_ll = ctx.get_link_layer_addr(device_id);
        let dst_ip = lookup_addr.to_solicited_node_address();
        // TODO(https://fxbug.dev/85055): Either panic or guarantee that this
        // error can't happen statically?
        let _ = send_ndp_packet::<_, _, &[u8], _>(
            ctx,
            device_id,
            src_ip.get(),
            dst_ip.into_specified(),
            NeighborSolicitation::new(lookup_addr.get()),
            &[NdpOptionBuilder::SourceLinkLayerAddress(src_ll.bytes())],
        );
    } else {
        // Nothing can be done if we don't have any ipv6 addresses to send
        // packets out to.
        debug!("Not sending NDP request, since we don't know our IPv6 address");
    }
}

fn send_neighbor_advertisement<D: LinkDevice, C: NdpContext<D>>(
    ctx: &mut C,
    device_id: C::DeviceId,
    solicited: bool,
    device_addr: SpecifiedAddr<Ipv6Addr>,
    dst_ip: SpecifiedAddr<Ipv6Addr>,
) {
    debug!("send_neighbor_advertisement from {:?} to {:?}", device_addr, dst_ip);
    debug_assert!(device_addr.is_valid_unicast());
    // We currently only allow the destination address to be:
    // 1) a unicast address.
    // 2) a multicast destination but the message should be a unsolicited
    //    neighbor advertisement.
    // NOTE: this assertion may need change if more messages are to be allowed in the future.
    debug_assert!(dst_ip.is_valid_unicast() || (!solicited && dst_ip.is_multicast()));

    // We must call into the higher level send_ndp_packet function because it is
    // not guaranteed that we have actually saved the link layer address of the
    // destination IP. Typically, the solicitation request will carry that
    // information, but it is not necessary. So it is perfectly valid that
    // trying to send this advertisement will end up triggering a neighbor
    // solicitation to be sent.
    let src_ll = ctx.get_link_layer_addr(device_id);
    // TODO(https://fxbug.dev/85055): Either panic or guarantee that this error
    // can't happen statically?
    let device_addr = device_addr.get();
    let _ = send_ndp_packet::<_, _, &[u8], _>(
        ctx,
        device_id,
        device_addr,
        dst_ip,
        NeighborAdvertisement::new(ctx.is_router_device(device_id), solicited, false, device_addr),
        &[NdpOptionBuilder::TargetLinkLayerAddress(src_ll.bytes())],
    );
}

/// Helper function to send MTU packet over an NdpDevice to `dst_ip`.
// TODO(https://fxbug.dev/85055): Is it possible to guarantee that some types of
// errors don't happen?
pub(super) fn send_ndp_packet<D: LinkDevice, C: NdpContext<D>, B: ByteSlice, M>(
    ctx: &mut C,
    device_id: C::DeviceId,
    src_ip: Ipv6Addr,
    dst_ip: SpecifiedAddr<Ipv6Addr>,
    message: M,
    options: &[NdpOptionBuilder<'_>],
) -> Result<(), ()>
where
    M: IcmpMessage<Ipv6, B, Code = IcmpUnusedCode>,
{
    trace!("send_ndp_packet: src_ip={:?} dst_ip={:?}", src_ip, dst_ip);

    ctx.send_ipv6_frame(
        device_id,
        dst_ip,
        ndp::OptionSequenceBuilder::<_>::new(options.iter())
            .into_serializer()
            .encapsulate(IcmpPacketBuilder::<Ipv6, B, M>::new(
                src_ip,
                dst_ip,
                IcmpUnusedCode,
                message,
            ))
            .encapsulate(Ipv6PacketBuilder::new(
                src_ip,
                dst_ip,
                REQUIRED_NDP_IP_PACKET_HOP_LIMIT,
                Ipv6Proto::Icmpv6,
            )),
    )
    .map_err(|_| ())
}

/// Execute the algorithm in RFC 4862 Section 5.5.3, adding or updating a static SLAAC addresses for
/// the given prefix.
fn apply_slaac_update<'a, D: LinkDevice, C: NdpContext<D>>(
    ctx: &mut C,
    device_id: <C as DeviceIdContext<D>>::DeviceId,
    prefix_info: &PrefixInformation,
) {
    if prefix_info.preferred_lifetime() > prefix_info.valid_lifetime() {
        // If the preferred lifetime is greater than the valid lifetime, silently ignore the Prefix
        // Information option, as per RFC 4862 section 5.5.3.
        trace!("receive_ndp_packet: autonomous prefix's preferred lifetime is greater than valid lifetime, ignoring");
        return;
    }

    let subnet = match Subnet::new(*prefix_info.prefix(), prefix_info.prefix_length()) {
        Ok(subnet) => subnet,
        Err(err) => {
            trace!(
                "receive_ndp_packet: autonomous prefix {:?} with length {:?} is not valid: {:?}",
                prefix_info.prefix(),
                prefix_info.prefix_length(),
                err
            );
            return;
        }
    };

    let now = ctx.now();
    let preferred_until =
        prefix_info.preferred_lifetime().map(|l| now.checked_add(l.get()).unwrap());

    let valid_for = prefix_info.valid_lifetime().map(|l| l.get()).unwrap_or(Duration::from_secs(0));
    let valid_until = now.checked_add(valid_for).unwrap();

    // Before configuring a SLAAC address, check to see if we already have a SLAAC address for the
    // given prefix.
    let entry = ctx
        .get_ip_device_state_mut(device_id)
        .iter_global_ipv6_addrs_mut()
        .find(|a| a.addr_sub().subnet() == subnet && a.config_type() == AddrConfigType::Slaac);
    if let Some(entry) = entry {
        let addr_sub = entry.addr_sub();
        let addr = addr_sub.addr();

        trace!("receive_ndp_packet: autonomous prefix is for an already configured SLAAC address {:?} on device {:?}", addr_sub, device_id);

        // TODO(https://fxbug.dev/91300): The code below assumes that valid_until is always finite,
        // which it is not. This means the `unwrap` can panic.
        let entry_valid_until = entry.valid_until().unwrap();
        let remaining_lifetime = if entry_valid_until < now {
            None
        } else {
            Some(entry_valid_until.duration_since(now))
        };

        // As per RFC 4862 section 5.5.3.e, if the advertised prefix is equal to the prefix of an
        // address configured by stateless autoconfiguration in the list, the preferred lifetime of
        // the address is reset to the Preferred Lifetime in the received advertisement.
        trace!("receive_ndp_packet: updating preferred lifetime to {:?} for SLAAC address {:?} on device {:?}", preferred_until, addr, device_id);

        // Update the preferred lifetime for this address.
        //
        // Must not have reached this point if the address was not already assigned to a device.
        if let Some(preferred_until_duration) = preferred_until {
            entry.state = match entry.state {
                AddressState::Deprecated => AddressState::Assigned,
                state => state,
            };
            let _: Option<C::Instant> = ctx.schedule_timer_instant(
                preferred_until_duration,
                NdpTimerId::new_deprecate_slaac_address(device_id, addr).into(),
            );
        } else if !entry.state.is_deprecated() {
            ctx.deprecate_slaac_addr(device_id, &addr);
            let _: Option<C::Instant> =
                ctx.cancel_timer(NdpTimerId::new_deprecate_slaac_address(device_id, addr));
        }

        // As per RFC 4862 section 5.5.3.e, the specific action to perform for the valid lifetime
        // of the address depends on the Valid Lifetime in the received advertisement and the
        // remaining time to the valid lifetime expiration of the previously autoconfigured
        // address:
        if (valid_for > MIN_PREFIX_VALID_LIFETIME_FOR_UPDATE)
            || remaining_lifetime.map_or(true, |r| r < valid_for)
        {
            // If the received Valid Lifetime is greater than 2 hours or greater than
            // RemainingLifetime, set the valid lifetime of the corresponding address to the
            // advertised Valid Lifetime.
            trace!("receive_ndp_packet: updating valid lifetime to {:?} for SLAAC address {:?} on device {:?}", valid_until, addr, device_id);

            // Set the valid lifetime for this address.
            ctx.update_slaac_addr_valid_until(device_id, &addr, valid_until);

            // Must not have reached this point if the address was already assigned to a device.
            assert_matches!(
                ctx.schedule_timer_instant(
                    valid_until,
                    NdpTimerId::new_invalidate_slaac_address(device_id, addr).into(),
                ),
                Some(_)
            );
        } else if remaining_lifetime.map_or(true, |r| r <= MIN_PREFIX_VALID_LIFETIME_FOR_UPDATE) {
            // If RemainingLifetime is less than or equal to 2 hours, ignore the Prefix Information
            // option with regards to the valid lifetime, unless the Router Advertisement from
            // which this option was obtained has been authenticated (e.g., via Secure Neighbor
            // Discovery [RFC3971]).  If the Router Advertisement was authenticated, the valid
            // lifetime of the corresponding address should be set to the Valid Lifetime in the
            // received option.
            //
            // TODO(ghanan): If the NDP packet this prefix option is in was authenticated, update
            //               the valid lifetime of the address to the valid lifetime in the
            //               received option, as per RFC 4862 section 5.5.3.e.
            trace!("receive_ndp_packet: not updating valid lifetime for SLAAC address {:?} on device {:?} as remaining lifetime is less than 2 hours and new valid lifetime ({:?}) is less than remaining lifetime", addr, device_id, valid_for);
        } else {
            // Otherwise, reset the valid lifetime of the corresponding address to 2 hours.
            trace!("receive_ndp_packet: resetting valid lifetime to 2 hrs for SLAAC address {:?} on device {:?}",addr, device_id);

            // Update the valid lifetime for this address.
            let valid_until = now.checked_add(MIN_PREFIX_VALID_LIFETIME_FOR_UPDATE).unwrap();

            ctx.update_slaac_addr_valid_until(device_id, &addr, valid_until);

            // Must not have reached this point if the address was not already assigned to a
            // device.
            assert_matches!(
                ctx.schedule_timer_instant(
                    valid_until,
                    NdpTimerId::new_invalidate_slaac_address(device_id, addr).into(),
                ),
                Some(_)
            );
        }
    } else {
        // As per RFC 4862 section 5.5.3.e, if the prefix advertised is not equal to the prefix of
        // an address configured by stateless autoconfiguration already in the list of addresses
        // associated with the interface, and if the Valid Lifetime is not 0, form an address (and
        // add it to the list) by combining the advertised prefix with an interface identifier of
        // the link as follows:
        //
        // |    128 - N bits    |        N bits          |
        // +--------------------+------------------------+
        // |    link prefix     |  interface identifier  |
        // +---------------------------------------------+
        if valid_for == ZERO_DURATION {
            trace!("receive_ndp_packet: autonomous prefix has valid lifetime = 0, ignoring");
            return;
        }

        if subnet.prefix() != REQUIRED_PREFIX_BITS {
            // If the sum of the prefix length and interface identifier length does not equal 128
            // bits, the Prefix Information option MUST be ignored, as per RFC 4862 section 5.5.3.
            error!("receive_ndp_packet: autonomous prefix length {:?} and interface identifier length {:?} cannot form valid IPv6 address, ignoring", subnet.prefix(), REQUIRED_PREFIX_BITS);
            return;
        }

        // Generate the global address as defined by RFC 4862 section 5.5.3.d.
        let address =
            generate_global_address(&subnet, &ctx.get_interface_identifier(device_id)[..]);

        // TODO(https://fxbug.dev/91301): Should bindings be the one to actually assign the address
        // to maintain a "single source of truth"?

        // Attempt to add the address to the device.
        if let Err(err) = ctx.add_slaac_addr_sub(device_id, address, valid_until) {
            error!("receive_ndp_packet: Failed configure new IPv6 address {:?} on device {:?} via SLAAC with error {:?}", address, device_id, err);
        } else {
            trace!("receive_ndp_packet: Successfully configured new IPv6 address {:?} on device {:?} via SLAAC", address, device_id);

            // Set the valid lifetime for this address.
            //
            // Must not have reached this point if the address was already assigned to a device.
            assert_eq!(
                ctx.schedule_timer_instant(
                    valid_until,
                    NdpTimerId::new_invalidate_slaac_address(device_id, address.addr()).into(),
                ),
                None
            );

            let timer_id = NdpTimerId::new_deprecate_slaac_address(device_id, address.addr());

            // Set the preferred lifetime for this address.
            //
            // Must not have reached this point if the address was already assigned to a device.
            match preferred_until {
                Some(preferred_until_duration) => assert_eq!(
                    ctx.schedule_timer_instant(preferred_until_duration, timer_id.into()),
                    None
                ),
                None => {
                    ctx.deprecate_slaac_addr(device_id, &address.addr());
                    assert_eq!(ctx.cancel_timer(timer_id.into()), None);
                }
            };
        }
    }
}

/// A handler for incoming NDP packets.
///
/// An implementation of `NdpPacketHandler` is provided by the device layer (see
/// the `crate::device` module) to the IP layer so that it can pass incoming NDP
/// packets. It can also be mocked for use in testing.
pub(crate) trait NdpPacketHandler<DeviceId> {
    /// Receive an NDP packet.
    fn receive_ndp_packet<B: ByteSlice>(
        &mut self,
        device: DeviceId,
        src_ip: Ipv6SourceAddr,
        dst_ip: SpecifiedAddr<Ipv6Addr>,
        packet: NdpPacket<B>,
    );
}

pub(crate) fn receive_ndp_packet<D: LinkDevice, C: NdpContext<D>, B>(
    ctx: &mut C,
    device_id: C::DeviceId,
    src_ip: Ipv6SourceAddr,
    _dst_ip: SpecifiedAddr<Ipv6Addr>,
    packet: NdpPacket<B>,
) where
    B: ByteSlice,
{
    // TODO(ghanan): Make sure the IP packet's hop limit was set to 255 as per
    //               RFC 4861 sections 4.1, 4.2, 4.3, 4.4, and 4.5 (each type of
    //               NDP packet).

    match packet {
        NdpPacket::RouterSolicitation(p) => {
            let _: IcmpPacket<Ipv6, B, RouterSolicitation> = p;

            trace!("receive_ndp_packet: Received NDP RS");

            if !ctx.is_router_device(device_id) {
                // Hosts MUST silently discard Router Solicitation messages as
                // per RFC 4861 section 6.1.1.
                trace!(
                    "receive_ndp_packet: device {:?} is not a router, discarding NDP RS",
                    device_id
                );
                return;
            }
        }
        NdpPacket::RouterAdvertisement(p) => {
            // Nodes MUST silently discard any received Router Advertisement
            // message where the IP source address is not a link-local
            // address as routers must use their link-local address as the
            // source for Router Advertisements so hosts can uniquely
            // identify routers, as per RFC 4861 section 6.1.2.
            let src_ip = match match src_ip {
                Ipv6SourceAddr::Unicast(ip) => LinkLocalUnicastAddr::new(ip),
                Ipv6SourceAddr::Unspecified => None,
            } {
                Some(ip) => {
                    trace!("receive_ndp_packet: NDP RA source={:?}", ip);
                    ip
                }
                None => {
                    trace!(
                        "receive_ndp_packet: NDP RA source={:?} is not link-local; discarding",
                        src_ip
                    );
                    return;
                }
            };

            // TODO(ghanan): Make sure IP's hop limit is set to 255 as per RFC
            // 4861 section 6.1.2.

            ctx.increment_counter("ndp::rx_router_advertisement");

            if ctx.is_router_device(device_id) {
                trace!("receive_ndp_packet: received NDP RA as a router, discarding NDP RA");
                return;
            }

            let ndp_state = ctx.get_state_mut_with(device_id);
            let ra = p.message();

            let timer_id = NdpTimerId::new_router_invalidation(device_id, src_ip).into();

            if let Some(router_lifetime) = ra.router_lifetime() {
                if ndp_state.has_default_router(&src_ip) {
                    trace!("receive_ndp_packet: NDP RA from an already known router: {:?}", src_ip);
                } else {
                    trace!("receive_ndp_packet: NDP RA from a new router: {:?}", src_ip);

                    // TODO(ghanan): Make the number of default routers we store
                    // configurable?
                    ndp_state.add_default_router(src_ip);
                };

                // Reset invalidation timeout.
                trace!("receive_ndp_packet: NDP RA: updating invalidation timer to {:?} for router: {:?}", router_lifetime, src_ip);
                let _: Option<C::Instant> = ctx.schedule_timer(router_lifetime.get(), timer_id);
            } else {
                if ndp_state.has_default_router(&src_ip) {
                    trace!("receive_ndp_packet: NDP RA has zero-valued router lifetime, invaliding router: {:?}", src_ip);

                    // `invalidate_default_router` may panic if `src_ip` does
                    // not reference a known default router, but we will only
                    // reach here if the router is already in our list of
                    // default routers, so we know `invalidate_default_router`
                    // will not panic.
                    ndp_state.invalidate_default_router(&src_ip);

                    // As per RFC 4861 section 6.3.4, immediately timeout the
                    // entry as specified in RFC 4861 section 6.3.5.
                    assert_matches!(ctx.cancel_timer(timer_id), Some(_));
                } else {
                    trace!("receive_ndp_packet: NDP RA has zero-valued router lifetime, but the router {:?} is unknown so doing nothing", src_ip);
                }

                // As per RFC 4861 section 4.2, a zero-valued router lifetime
                // only indicates the router is not to be used as a default
                // router and is only applied to its usefulness as a default
                // router; it does not apply to the other information contained
                // in this message's fields or options. Given this, we continue
                // as normal.
            }

            // Borrow again so that a) we shadow the original `ndp_state` and
            // thus, b) the original is dropped before `ctx` is used mutably in
            // various code above (namely, to schedule timers). Now that all of
            // that mutation has happened, we can borrow `ctx` mutably again and
            // not run afoul of the borrow checker.
            let ndp_state = ctx.get_state_mut_with(device_id);

            // As per RFC 4861 section 6.3.4:
            // If the received Reachable Time value is specified, the host
            // SHOULD set its BaseReachableTime variable to the received value.
            // If the new value differs from the previous value, the host SHOULD
            // re-compute a new random ReachableTime value.
            //
            // TODO(ghanan): Make the updating of this field from the RA message
            //               configurable since the RFC does not say we MUST
            //               update the field.
            //
            // TODO(ghanan): In most cases, the advertised Reachable Time value
            //               will be the same in consecutive Router
            //               Advertisements, and a host's BaseReachableTime
            //               rarely changes.  In such cases, an implementation
            //               SHOULD ensure that a new random value gets
            //               re-computed at least once every few hours.
            if let Some(base_reachable_time) = ra.reachable_time() {
                trace!("receive_ndp_packet: NDP RA: updating base_reachable_time to {:?} for router: {:?}", base_reachable_time, src_ip);
                ndp_state.set_base_reachable_time(base_reachable_time.get());
            }

            // As per RFC 4861 section 6.3.4:
            // The RetransTimer variable SHOULD be copied from the Retrans Timer
            // field, if it is specified.
            //
            // TODO(ghanan): Make the updating of this field from the RA message
            //               configurable since the RFC does not say we MUST
            //               update the field.
            if let Some(retransmit_timer) = ra.retransmit_timer() {
                trace!(
                    "receive_ndp_packet: NDP RA: updating retrans_timer to {:?} for router: {:?}",
                    retransmit_timer,
                    src_ip
                );
                ndp_state.set_retrans_timer(retransmit_timer.get());
            }

            // As per RFC 4861 section 6.3.4:
            // If the received Cur Hop Limit value is specified, the host SHOULD
            // set its CurHopLimit variable to the received value.
            //
            // TODO(ghanan): Make the updating of this field from the RA message
            //               configurable since the RFC does not say we MUST
            //               update the field.
            if let Some(hop_limit) = ra.current_hop_limit() {
                trace!("receive_ndp_packet: NDP RA: updating device's hop limit to {:?} for router: {:?}", ra.current_hop_limit(), src_ip);

                ctx.get_ip_device_state_mut(device_id).default_hop_limit = hop_limit;
            }

            for option in p.body().iter() {
                match option {
                    // As per RFC 4861 section 6.3.4, if a Neighbor Cache entry
                    // is created for the router, its reachability state MUST be
                    // set to STALE as specified in Section 7.3.3.  If a cache
                    // entry already exists and is updated with a different
                    // link-layer address, the reachability state MUST also be
                    // set to STALE.
                    //
                    // TODO(ghanan): Mark NDP state as STALE as per the RFC once
                    //               we implement the RFC compliant states.
                    NdpOption::SourceLinkLayerAddress(a) => {
                        let ndp_state = ctx.get_state_mut_with(device_id);
                        let link_addr = D::Address::from_bytes(&a[..D::Address::BYTES_LENGTH]);

                        trace!("receive_ndp_packet: NDP RA: setting link address for router {:?} to {:?}", src_ip, link_addr);

                        // Set the link address and mark it as stale if we
                        // either created the neighbor entry, or updated an
                        // existing one.
                        ndp_state.neighbors.set_link_address(src_ip.get(), link_addr, false);
                    }
                    NdpOption::Mtu(mtu) => {
                        trace!("receive_ndp_packet: mtu option with mtu = {:?}", mtu);

                        // TODO(ghanan): Make updating the MTU from an RA
                        // message configurable.
                        if mtu >= Ipv6::MINIMUM_LINK_MTU.into() {
                            // `set_mtu` may panic if `mtu` is less than
                            // `MINIMUM_LINK_MTU` but we just checked to make
                            // sure that `mtu` is at least `MINIMUM_LINK_MTU` so
                            // we know `set_mtu` will not panic.
                            ctx.set_mtu(device_id, mtu);
                        } else {
                            trace!("receive_ndp_packet: NDP RA: not setting link MTU (from {:?}) to {:?} as it is less than Ipv6::MINIMUM_LINK_MTU", src_ip, mtu);
                        }
                    }
                    NdpOption::PrefixInformation(prefix_info) => {
                        let ndp_state = ctx.get_state_mut_with(device_id);

                        trace!(
                            "receive_ndp_packet: prefix information option with prefix = {:?}",
                            prefix_info
                        );

                        let subnet = match prefix_info.subnet() {
                            Ok(subnet) => match UnicastAddr::new(subnet.network()) {
                                Some(_) => subnet,
                                None => {
                                    trace!("receive_ndp_packet: invalid non-unicast prefix ({:?}), so ignoring", subnet);
                                    continue;
                                }
                            },
                            Err(err) => {
                                trace!("receive_ndp_packet: malformed prefix information ({:?}), so ignoring", err);
                                continue;
                            }
                        };

                        if prefix_info.prefix().is_link_local() {
                            // As per RFC 4861 section 6.3.4 (on-link prefix
                            // determination) and RFC 4862 section 5.5.3
                            // (SLAAC), ignore options with the the link-local
                            // prefix.
                            trace!("receive_ndp_packet: prefix is a link local, so ignoring");
                            continue;
                        }

                        if prefix_info.on_link_flag() {
                            // Timer ID for this prefix's invalidation.
                            let timer_id =
                                NdpTimerId::new_prefix_invalidation(device_id, subnet).into();

                            if let Some(valid_lifetime) = prefix_info.valid_lifetime() {
                                if !ndp_state.has_prefix(&subnet) {
                                    // `add_prefix` may panic if the prefix
                                    // already exists in our prefix list, but we
                                    // will only reach here if it doesn't so we
                                    // know `add_prefix` will not panic.
                                    ndp_state.add_prefix(subnet);
                                }

                                // Reset invalidation timer.
                                if valid_lifetime == INFINITE_LIFETIME {
                                    // We do not need a timer to mark the prefix
                                    // as invalid when it has an infinite
                                    // lifetime.
                                    let _: Option<C::Instant> = ctx.cancel_timer(timer_id);
                                } else {
                                    let _: Option<C::Instant> =
                                        ctx.schedule_timer(valid_lifetime.get(), timer_id);
                                }
                            } else if ndp_state.has_prefix(&subnet) {
                                trace!("receive_ndp_packet: on-link prefix is known and has valid lifetime = 0, so invaliding");

                                // If the on-link flag is set, the valid
                                // lifetime is 0 and the prefix is already
                                // present in our prefix list, timeout the
                                // prefix immediately, as per RFC 4861 section
                                // 6.3.4.

                                // Cancel the prefix invalidation timeout if it
                                // exists.
                                let _: Option<C::Instant> = ctx.cancel_timer(timer_id);

                                let ndp_state = ctx.get_state_mut_with(device_id);
                                ndp_state.invalidate_prefix(subnet);
                            } else {
                                // If the on-link flag is set, the valid
                                // lifetime is 0 and the prefix is not present
                                // in our prefix list, ignore the option, as per
                                // RFC 4861 section 6.3.4.
                                trace!("receive_ndp_packet: on-link prefix is unknown and is has valid lifetime = 0, so ignoring");
                            }
                        }

                        if prefix_info.autonomous_address_configuration_flag() {
                            apply_slaac_update(ctx, device_id, prefix_info);
                        }
                    }
                    _ => {}
                }
            }

            // If the router exists in our router table, make sure it is marked
            // as a router as per RFC 4861 section 6.3.4.
            let ndp_state = ctx.get_state_mut_with(device_id);
            if let Some(state) = ndp_state.neighbors.get_neighbor_state_mut(&src_ip) {
                state.is_router = true;
            }
        }
        NdpPacket::NeighborSolicitation(p) => {
            let target_address = p.message().target_address();
            let target_address = match UnicastAddr::new(*target_address) {
                Some(addr) => {
                    trace!("receive_ndp_packet: NDP NS target={:?}", addr);
                    addr
                }
                None => {
                    trace!(
                        "receive_ndp_packet: NDP NS target={:?} is not unicast; discarding",
                        target_address
                    );
                    return;
                }
            };

            // Is `target_address` associated with our device? If not, drop the
            // packet.
            if ctx.get_ip_device_state(device_id).find_addr(&target_address).is_none() {
                trace!("receive_ndp_packet: Dropping NDP NS packet that is not meant for us");
                return;
            }

            // We know the call to `unwrap` will not panic because we just
            // checked to make sure that `target_address` is associated with
            // `device_id`.
            let state =
                ctx.get_ip_device_state(device_id).find_addr(&target_address).unwrap().state;
            if state.is_tentative() {
                if !src_ip.is_specified() {
                    // If the source address of the packet is the unspecified
                    // address, the source of the packet is performing DAD for
                    // the same target address as our `my_addr`. A duplicate
                    // address has been detected.
                    trace!(
                        "receive_ndp_packet: Received NDP NS: duplicate address {:?} detected on device {:?}", target_address, device_id
                    );

                    ctx.duplicate_address_detected(device_id, target_address);
                }

                // `target_address` is tentative on `device_id` so we do not
                // continue processing the NDP NS.
                return;
            }

            // At this point, we guarantee the following is true because of the
            // earlier checks:
            //
            //   1) The target address is a valid unicast address.
            //   2) The target address is an address that is on our device,
            //      `device_id`.
            //   3) The target address is not tentative.

            ctx.increment_counter("ndp::rx_neighbor_solicitation");

            // If we have a source link layer address option, we take it and
            // save to our cache.
            if let Ipv6SourceAddr::Unicast(src_ip) = src_ip {
                // We only update the cache if it is not from an unspecified
                // address, i.e., it is not a DAD message. (RFC 4861)
                if let Some(ll) = get_source_link_layer_option(p.body()) {
                    trace!("receive_ndp_packet: Received NDP NS from {:?} has source link layer option w/ link address {:?}", src_ip, ll);

                    // Set the link address and mark it as stale if we either
                    // create the neighbor entry, or updated an existing one, as
                    // per RFC 4861 section 7.2.3.
                    ctx.get_state_mut_with(device_id).neighbors.set_link_address(src_ip, ll, false);
                }

                trace!(
                    "receive_ndp_packet: Received NDP NS: sending NA to source of NS {:?}",
                    src_ip
                );

                // Finally we ought to reply to the Neighbor Solicitation with a
                // Neighbor Advertisement.
                send_neighbor_advertisement(
                    ctx,
                    device_id,
                    true,
                    target_address.into_specified(),
                    src_ip.into_specified(),
                );
            } else {
                trace!("receive_ndp_packet: Received NDP NS: sending NA to all nodes multicast");

                // Send out Unsolicited Advertisement in response to neighbor
                // who's performing DAD, as described in RFC 4861 and 4862.
                send_neighbor_advertisement(
                    ctx,
                    device_id,
                    false,
                    target_address.into_specified(),
                    Ipv6::ALL_NODES_LINK_LOCAL_MULTICAST_ADDRESS.into_specified(),
                )
            }
        }
        NdpPacket::NeighborAdvertisement(p) => {
            let message = p.message();
            let target_address = p.message().target_address();

            let (src_ip, target_address) = match (src_ip, UnicastAddr::new(*target_address)) {
                (Ipv6SourceAddr::Unicast(src_ip), Some(target_address)) => {
                    trace!(
                        "receive_ndp_packet: NDP NA source={:?} target={:?}",
                        src_ip,
                        target_address
                    );
                    (src_ip, target_address)
                }
                (Ipv6SourceAddr::Unspecified, Some(target_address)) => {
                    trace!("receive_ndp_packet: NDP NA source={:?} target={:?}; source is not specified; discarding", src_ip, target_address);
                    return;
                }
                (Ipv6SourceAddr::Unicast(src_ip), None) => {
                    trace!("receive_ndp_packet: NDP NA source={:?} target={:?}; target is not unicast; discarding", src_ip, target_address);
                    return;
                }
                (Ipv6SourceAddr::Unspecified, None) => {
                    trace!("receive_ndp_packet: NDP NA source={:?} target={:?}; source is not specified and target is not unicast; discarding", src_ip, target_address);
                    return;
                }
            };

            match ctx
                .get_ip_device_state(device_id)
                .find_addr(&target_address)
                .map(|entry| entry.state)
            {
                Some(AddressState::Tentative { dad_transmits_remaining }) => {
                    trace!("receive_ndp_packet: NDP NA has a target address {:?} that is tentative on device {:?}; dad_transmits_remaining={:?}", target_address, device_id, dad_transmits_remaining);
                    ctx.duplicate_address_detected(device_id, target_address);
                    return;
                }
                Some(AddressState::Assigned) | Some(AddressState::Deprecated) => {
                    // RFC 4862 says this situation is out of the scope, so we
                    // just log out the situation for now.
                    //
                    // TODO(ghanan): Signal to bindings that a duplicate address
                    // is detected?
                    error!("receive_ndp_packet: NDP NA: A duplicated address {:?} found on device {:?} when we are not in DAD process!", target_address, device_id);
                    return;
                }
                // Do nothing.
                None => {}
            }

            ctx.increment_counter("ndp::rx_neighbor_advertisement");

            let ndp_state = ctx.get_state_mut_with(device_id);

            let neighbor_state = if let Some(state) =
                ndp_state.neighbors.get_neighbor_state_mut(&src_ip)
            {
                state
            } else {
                // If the neighbor is not in the cache, we just ignore the
                // advertisement, as we're not yet interested in communicating
                // with it, as per RFC 4861 section 7.2.5.
                trace!("receive_ndp_packet: Ignoring NDP NA from {:?} does not already exist in our list of neighbors, so discarding", src_ip);
                return;
            };

            let target_ll = get_target_link_layer_option(p.body());

            if neighbor_state.is_incomplete() {
                // If we are in the Incomplete state, we should not have ever
                // learned about a link-layer address.
                assert_eq!(neighbor_state.link_address, None);

                if let Some(address) = target_ll {
                    // Set the IsRouter flag as per RFC 4861 section 7.2.5.
                    trace!(
                        "receive_ndp_packet: NDP RS from {:?} indicates it {:?} a router",
                        src_ip,
                        if message.router_flag() { "is" } else { "isn't" }
                    );
                    neighbor_state.is_router = message.router_flag();

                    // Record the link-layer address.
                    //
                    // If the advertisement's Solicited flag is set, the state
                    // of the entry is set to REACHABLE; otherwise, it is set to
                    // STALE, as per RFC 4861 section 7.2.5.
                    //
                    // Note, since the neighbor's link address was `None`
                    // before, we will definitely update the address, so the
                    // state will be set to STALE if the solicited flag is
                    // unset.
                    trace!(
                        "receive_ndp_packet: Resolving link address of {:?} to {:?}",
                        src_ip,
                        address
                    );
                    ndp_state.neighbors.set_link_address(src_ip, address, message.solicited_flag());

                    // Cancel the resolution timeout.
                    let _: Option<C::Instant> = ctx.cancel_timer(
                        NdpTimerId::new_link_address_resolution(device_id, src_ip).into(),
                    );

                    // Send any packets queued for the neighbor awaiting address
                    // resolution.
                    ctx.address_resolved(device_id, &src_ip, address);
                } else {
                    trace!("receive_ndp_packet: Performing address resolution but the NDP NA from {:?} does not have a target link layer address option, so discarding", src_ip);
                    return;
                }

                return;
            }

            // If we are not in the Incomplete state, we should have (at some
            // point) learned about a link-layer address.
            assert_matches!(neighbor_state.link_address, Some(_));

            if !message.override_flag() {
                // As per RFC 4861 section 7.2.5:
                //
                // If the Override flag is clear and the supplied link-layer
                // address differs from that in the cache, then one of two
                // actions takes places:
                //
                // a) If the state of the entry is REACHABLE, set it to STALE,
                //    but do not update the entry in any other way.
                //
                // b) Otherwise, the received advertisement should be ignored
                //    and MUST NOT update cache.
                if target_ll.map_or(false, |x| neighbor_state.link_address != Some(x)) {
                    if neighbor_state.is_reachable() {
                        trace!("receive_ndp_packet: NDP RS from known reachable neighbor {:?} does not have override set, but supplied link addr is different, setting state to stale", src_ip);
                        neighbor_state.state = NeighborEntryState::Stale;
                    } else {
                        trace!("receive_ndp_packet: NDP RS from known neighbor {:?} (with reachability unknown) does not have override set, but supplied link addr is different, ignoring", src_ip);
                    }
                }
            }

            // Ignore this unless `target_ll` is `Some`.
            let mut is_same = false;

            // If override is set, the link-layer address MUST be inserted into
            // the cache (if one is supplied and differs from the already
            // recoded address).
            if let Some(address) = target_ll {
                let address = Some(address);

                is_same = neighbor_state.link_address == address;

                if !is_same && message.override_flag() {
                    neighbor_state.link_address = address;
                }
            }

            // If the override flag is set, or the supplied link-layer address
            // is the same as that in the cache, or no Target Link-Layer Address
            // option was supplied:
            if message.override_flag() || target_ll.is_none() || is_same {
                // - If the solicited flag is set, the state of the entry MUST
                //   be set to REACHABLE.
                // - Else, if it was unset, and the link address was updated,
                //   the state MUST be set to STALE.
                // - Otherwise, the state remains the same.
                if message.solicited_flag() {
                    trace!("receive_ndp_packet: NDP RS from {:?} is solicited and either has override set, link address isn't provided, or the provided address is not different, updating state to Reachable", src_ip);
                    neighbor_state.state = NeighborEntryState::Reachable;
                } else if message.override_flag() && target_ll.is_some() && !is_same {
                    trace!("receive_ndp_packet: NDP RS from {:?} is unsolicited and the link address was updated, updating state to Stale", src_ip);

                    neighbor_state.state = NeighborEntryState::Stale;
                } else {
                    trace!("receive_ndp_packet: NDP RS from {:?} is unsolicited and the link address was not updated, doing nothing", src_ip);
                }

                // Check if the neighbor transitioned from a router -> host.
                if neighbor_state.is_router && !message.router_flag() {
                    trace!("receive_ndp_packet: NDP RS from {:?} informed us that it is no longer a router, updating is_router flag", src_ip);
                    neighbor_state.is_router = false;

                    if let Some(router_ll) = LinkLocalUnicastAddr::new(src_ip) {
                        // Invalidate the router as a default router if it is
                        // one of our default routers.
                        if ndp_state.has_default_router(&router_ll) {
                            trace!("receive_ndp_packet: NDP RS from {:?} (known as a default router) informed us that it is no longer a router, invaliding the default router", src_ip);
                            ndp_state.invalidate_default_router(&router_ll);
                        }
                    }
                } else {
                    neighbor_state.is_router = message.router_flag();
                }
            }
        }
        NdpPacket::Redirect(_) => log_unimplemented!((), "NDP Redirect not implemented"),
    }
}

fn get_source_link_layer_option<L: LinkAddress, B>(options: &Options<B>) -> Option<L>
where
    B: ByteSlice,
{
    options.iter().find_map(|o| match o {
        NdpOption::SourceLinkLayerAddress(a) => {
            if a.len() >= L::BYTES_LENGTH {
                Some(L::from_bytes(&a[..L::BYTES_LENGTH]))
            } else {
                None
            }
        }
        _ => None,
    })
}

fn get_target_link_layer_option<L: LinkAddress, B>(options: &Options<B>) -> Option<L>
where
    B: ByteSlice,
{
    options.iter().find_map(|o| match o {
        NdpOption::TargetLinkLayerAddress(a) => {
            if a.len() >= L::BYTES_LENGTH {
                Some(L::from_bytes(&a[..L::BYTES_LENGTH]))
            } else {
                None
            }
        }
        _ => None,
    })
}

/// Generate an IPv6 Global Address as defined by RFC 4862 section 5.5.3.d.
///
/// The generated address will be of the format:
///
/// |            128 - N bits               |       N bits           |
/// +---------------------------------------+------------------------+
/// |            link prefix                |  interface identifier  |
/// +----------------------------------------------------------------+
///
/// # Panics
///
/// Panics if a valid IPv6 unicast address cannot be formed with the provided
/// prefix and interface identifier, or if the prefix length is not a multiple
/// of 8 bits.
fn generate_global_address(
    prefix: &Subnet<Ipv6Addr>,
    iid: &[u8],
) -> AddrSubnet<Ipv6Addr, UnicastAddr<Ipv6Addr>> {
    if prefix.prefix() % 8 != 0 {
        unimplemented!("generate_global_address: not implemented for when prefix length is not a multiple of 8 bits");
    }

    let prefix_len = usize::from(prefix.prefix() / 8);

    assert_eq!(usize::from(Ipv6Addr::BYTES) - prefix_len, iid.len());

    let mut address = prefix.network().ipv6_bytes();
    address[prefix_len..].copy_from_slice(&iid);

    let address = AddrSubnet::new(Ipv6Addr::from(address), prefix.prefix()).unwrap();
    assert_eq!(address.subnet(), *prefix);

    address
}

#[cfg(test)]
mod tests {
    use super::*;

    use alloc::{vec, vec::Vec};
    use core::convert::{TryFrom, TryInto as _};

    use net_types::ethernet::Mac;
    use net_types::ip::AddrSubnet;
    use packet::{Buf, ParseBuffer};
    use packet_formats::icmp::ndp::{
        options::PrefixInformation, OptionSequenceBuilder, RouterAdvertisement, RouterSolicitation,
    };
    use packet_formats::icmp::{IcmpEchoRequest, Icmpv6Packet};
    use packet_formats::ip::IpProto;
    use packet_formats::testutil::{
        parse_ethernet_frame, parse_icmp_packet_in_ip_packet_in_ethernet_frame,
    };

    use crate::device::{
        add_ip_addr_subnet, del_ip_addr,
        ethernet::{EthernetLinkDevice, EthernetTimerId},
        set_routing_enabled, DeviceId, DeviceIdInner, DeviceLayerTimerId, DeviceLayerTimerIdInner,
        EthernetDeviceId,
    };
    use crate::testutil::{
        self, get_counter_val, run_for, set_logger_for_test, trigger_next_timer,
        DummyEventDispatcher, DummyEventDispatcherBuilder, DummyInstant, DummyNetwork, StepResult,
        TestIpExt, DUMMY_CONFIG_V6,
    };
    use crate::{
        assert_empty,
        context::InstantContext as _,
        ip::device::{
            get_assigned_ip_addr_subnets, get_ipv6_device_state, get_ipv6_hop_limit, get_mtu,
            is_routing_enabled, state::Ipv6AddressEntry,
        },
        Ctx, Instant, Ipv6StateBuilder, StackStateBuilder, TimerId, TimerIdInner,
    };

    type IcmpParseArgs = packet_formats::icmp::IcmpParseArgs<Ipv6Addr>;

    impl From<NdpTimerId<EthernetLinkDevice, EthernetDeviceId>> for TimerId {
        fn from(id: NdpTimerId<EthernetLinkDevice, EthernetDeviceId>) -> Self {
            TimerId(TimerIdInner::DeviceLayer(DeviceLayerTimerId(
                DeviceLayerTimerIdInner::Ethernet(EthernetTimerId::Ndp(id)),
            )))
        }
    }

    // TODO(https://github.com/rust-lang/rust/issues/67441): Make these constants once const
    // Option::unwrap is stablized
    fn local_mac() -> UnicastAddr<Mac> {
        UnicastAddr::new(Mac::new([0, 1, 2, 3, 4, 5])).unwrap()
    }

    fn remote_mac() -> UnicastAddr<Mac> {
        UnicastAddr::new(Mac::new([6, 7, 8, 9, 10, 11])).unwrap()
    }

    fn local_ip() -> UnicastAddr<Ipv6Addr> {
        UnicastAddr::from_witness(DUMMY_CONFIG_V6.local_ip).unwrap()
    }

    fn remote_ip() -> UnicastAddr<Ipv6Addr> {
        UnicastAddr::from_witness(DUMMY_CONFIG_V6.remote_ip).unwrap()
    }

    fn router_advertisement_message(
        src_ip: Ipv6Addr,
        dst_ip: Ipv6Addr,
        current_hop_limit: u8,
        managed_flag: bool,
        other_config_flag: bool,
        router_lifetime: u16,
        reachable_time: u32,
        retransmit_timer: u32,
    ) -> Buf<Vec<u8>> {
        Buf::new(Vec::new(), ..)
            .encapsulate(IcmpPacketBuilder::<Ipv6, &[u8], _>::new(
                src_ip,
                dst_ip,
                IcmpUnusedCode,
                RouterAdvertisement::new(
                    current_hop_limit,
                    managed_flag,
                    other_config_flag,
                    router_lifetime,
                    reachable_time,
                    retransmit_timer,
                ),
            ))
            .serialize_vec_outer()
            .unwrap()
            .into_inner()
    }

    fn neighbor_advertisement_message(
        src_ip: Ipv6Addr,
        dst_ip: Ipv6Addr,
        router_flag: bool,
        solicited_flag: bool,
        override_flag: bool,
        mac: Option<Mac>,
    ) -> Buf<Vec<u8>> {
        let mac = mac.map(|x| x.bytes());

        let mut options = Vec::new();

        if let Some(ref mac) = mac {
            options.push(NdpOptionBuilder::TargetLinkLayerAddress(mac));
        }

        OptionSequenceBuilder::new(options.iter())
            .into_serializer()
            .encapsulate(IcmpPacketBuilder::<Ipv6, &[u8], _>::new(
                src_ip,
                dst_ip,
                IcmpUnusedCode,
                NeighborAdvertisement::new(router_flag, solicited_flag, override_flag, src_ip),
            ))
            .serialize_vec_outer()
            .unwrap()
            .unwrap_b()
    }

    impl TryFrom<DeviceId> for EthernetDeviceId {
        type Error = DeviceId;
        fn try_from(id: DeviceId) -> Result<EthernetDeviceId, DeviceId> {
            match id.inner() {
                DeviceIdInner::Ethernet(id) => Ok(id),
                DeviceIdInner::Loopback => Err(id),
            }
        }
    }

    #[test]
    fn test_ndp_configuration() {
        let mut c = NdpConfiguration::default();
        let solicits = NonZeroU8::new(MAX_RTR_SOLICITATIONS);
        assert_eq!(c.get_max_router_solicitations(), solicits);

        let solicits = None;
        c.set_max_router_solicitations(solicits);
        assert_eq!(c.get_max_router_solicitations(), solicits);

        let solicits = NonZeroU8::new(2);
        c.set_max_router_solicitations(solicits);
        assert_eq!(c.get_max_router_solicitations(), solicits);

        // Max Router Solicitations gets saturated at `MAX_RTR_SOLICITATIONS`.
        c.set_max_router_solicitations(NonZeroU8::new(MAX_RTR_SOLICITATIONS + 1));
        assert_eq!(c.get_max_router_solicitations(), NonZeroU8::new(MAX_RTR_SOLICITATIONS));
    }

    #[test]
    fn test_send_neighbor_solicitation_on_cache_miss() {
        set_logger_for_test();
        let mut ctx = DummyEventDispatcherBuilder::default().build::<DummyEventDispatcher>();
        let dev_id =
            ctx.state.device.add_ethernet_device(local_mac(), Ipv6::MINIMUM_LINK_MTU.into());
        crate::device::initialize_device(&mut ctx, dev_id);
        // Now we have to manually assign the IP addresses, see
        // `EthernetLinkDevice::get_ipv6_addr`
        add_ip_addr_subnet(&mut ctx, dev_id, AddrSubnet::new(local_ip().get(), 128).unwrap())
            .unwrap();

        assert_eq!(
            lookup::<EthernetLinkDevice, _>(
                &mut ctx,
                dev_id.try_into().expect("expected ethernet ID"),
                remote_ip()
            ),
            None
        );

        // Check that we send the original neighbor solicitation, then resend a
        // few times if we don't receive a response.
        for packet_num in 0..usize::from(MAX_MULTICAST_SOLICIT) {
            assert_eq!(ctx.dispatcher.frames_sent().len(), packet_num + 1);

            assert_eq!(
                testutil::trigger_next_timer(&mut ctx).unwrap(),
                NdpTimerId::new_link_address_resolution(
                    dev_id.try_into().expect("expected ethernet ID"),
                    remote_ip()
                )
                .into()
            );
        }
        // Check that we hit the timeout after MAX_MULTICAST_SOLICIT.
        assert_eq!(
            *ctx.state.test_counters.get("ndp::neighbor_solicitation_timer"),
            1,
            "timeout counter at zero"
        );
    }

    #[test]
    fn test_address_resolution() {
        set_logger_for_test();
        let mut local = DummyEventDispatcherBuilder::default();
        assert_eq!(local.add_device(local_mac()), 0);
        let mut remote = DummyEventDispatcherBuilder::default();
        assert_eq!(remote.add_device(remote_mac()), 0);
        let device_id = DeviceId::new_ethernet(0);

        let mut net = DummyNetwork::new(
            vec![("local", local.build()), ("remote", remote.build())].into_iter(),
            |ctx, _| {
                if *ctx == "local" {
                    vec![("remote", device_id, None)]
                } else {
                    vec![("local", device_id, None)]
                }
            },
        );

        // Let's try to ping the remote device from the local device:
        let req = IcmpEchoRequest::new(0, 0);
        let req_body = &[1, 2, 3, 4];
        let body = Buf::new(req_body.to_vec(), ..).encapsulate(
            IcmpPacketBuilder::<Ipv6, &[u8], _>::new(local_ip(), remote_ip(), IcmpUnusedCode, req),
        );
        // Manually assigning the addresses.
        add_ip_addr_subnet(
            net.context("local"),
            device_id,
            AddrSubnet::new(local_ip().get(), 128).unwrap(),
        )
        .unwrap();
        add_ip_addr_subnet(
            net.context("remote"),
            device_id,
            AddrSubnet::new(remote_ip().get(), 128).unwrap(),
        )
        .unwrap();
        assert_empty(net.context("local").dispatcher.frames_sent());
        assert_empty(net.context("remote").dispatcher.frames_sent());

        crate::ip::send_ip_packet_from_device(
            net.context("local"),
            device_id,
            local_ip().get(),
            remote_ip().get(),
            remote_ip().into_specified(),
            Ipv6Proto::Icmpv6,
            body,
            None,
        )
        .unwrap();
        // This should've triggered a neighbor solicitation to come out of
        // local.
        assert_eq!(net.context("local").dispatcher.frames_sent().len(), 1);
        // A timer should've been started.
        assert_eq!(net.context("local").dispatcher.timer_events().count(), 1);

        let _: StepResult = net.step();
        // Neighbor entry for remote should be marked as Incomplete.
        assert_eq!(
            StateContext::<NdpState<EthernetLinkDevice>, _>::get_state_mut_with(
                net.context("local"),
                device_id.try_into().expect("expected ethernet ID")
            )
            .neighbors
            .get_neighbor_state(&remote_ip())
            .unwrap()
            .state,
            NeighborEntryState::Incomplete { transmit_counter: 1 }
        );

        assert_eq!(
            *net.context("remote").state.test_counters.get("ndp::rx_neighbor_solicitation"),
            1,
            "remote received solicitation"
        );
        assert_eq!(net.context("remote").dispatcher.frames_sent().len(), 1);

        // Forward advertisement response back to local.
        let _: StepResult = net.step();

        assert_eq!(
            *net.context("local").state.test_counters.get("ndp::rx_neighbor_advertisement"),
            1,
            "local received advertisement"
        );

        // At the end of the exchange, both sides should have each other in
        // their NDP tables.
        let local_neighbor = StateContext::<NdpState<EthernetLinkDevice>, _>::get_state_mut_with(
            net.context("local"),
            device_id.try_into().expect("expected ethernet ID"),
        )
        .neighbors
        .get_neighbor_state(&remote_ip())
        .unwrap();
        assert_eq!(local_neighbor.link_address.unwrap(), remote_mac().get(),);
        // Remote must be reachable from local since it responded with an NA
        // message with the solicited flag set.
        assert_eq!(local_neighbor.state, NeighborEntryState::Reachable,);

        let remote_neighbor = StateContext::<NdpState<EthernetLinkDevice>, _>::get_state_mut_with(
            net.context("remote"),
            device_id.try_into().expect("expected ethernet ID"),
        )
        .neighbors
        .get_neighbor_state(&local_ip())
        .unwrap();
        assert_eq!(remote_neighbor.link_address.unwrap(), local_mac().get(),);
        // Local must be marked as stale because remote got an NS from it but
        // has not itself sent any packets to it and confirmed that local
        // actually received it.
        assert_eq!(remote_neighbor.state, NeighborEntryState::Stale);

        // The local timer should've been unscheduled.
        assert_empty(net.context("local").dispatcher.timer_events());

        // Upon link layer resolution, the original ping request should've been
        // sent out.
        assert_eq!(net.context("local").dispatcher.frames_sent().len(), 1);
        let _: StepResult = net.step();
        assert_eq!(
            *net.context("remote").state.test_counters.get("<IcmpIpTransportContext as BufferIpTransportContext<Ipv6>>::receive_ip_packet::echo_request"),
            1
        );

        // TODO(brunodalbo): We should be able to verify that remote also sends
        //  back an echo reply, but we're having some trouble with IPv6 link
        //  local addresses.
    }

    #[test]
    fn test_deinitialize_cancels_timers() {
        // Test that associated timers are cancelled when the NDP device
        // is deinitialized.

        set_logger_for_test();
        let mut ctx = DummyEventDispatcherBuilder::default().build::<DummyEventDispatcher>();
        let dev_id =
            ctx.state.device.add_ethernet_device(local_mac(), Ipv6::MINIMUM_LINK_MTU.into());
        crate::device::initialize_device(&mut ctx, dev_id);
        // Now we have to manually assign the IP addresses, see
        // `EthernetLinkDevice::get_ipv6_addr`
        add_ip_addr_subnet(&mut ctx, dev_id, AddrSubnet::new(local_ip().get(), 128).unwrap())
            .unwrap();

        assert_eq!(
            lookup::<EthernetLinkDevice, _>(
                &mut ctx,
                dev_id.try_into().expect("expected ethernet ID"),
                remote_ip()
            ),
            None
        );

        // This should have scheduled a timer
        assert_eq!(ctx.dispatcher.timer_events().count(), 1);

        // Deinitializing a different ID should not impact the current timer
        let other_id = {
            let EthernetDeviceId(id) = dev_id.try_into().expect("expected ethernet ID");
            EthernetDeviceId(id + 1).into()
        };
        deinitialize(&mut ctx, other_id);
        assert_eq!(ctx.dispatcher.timer_events().count(), 1);

        // Deinitializing the correct ID should cancel the timer.
        deinitialize(&mut ctx, dev_id.try_into().expect("expected ethernet ID"));
        assert_empty(ctx.dispatcher.timer_events());
    }

    #[test]
    fn test_dad_duplicate_address_detected_solicitation() {
        // Tests whether a duplicate address will get detected by solicitation
        // In this test, two nodes having the same MAC address will come up at
        // the same time. And both of them will use the EUI address. Each of
        // them should be able to detect each other is using the same address,
        // so they will both give up using that address.
        set_logger_for_test();
        let mac = UnicastAddr::new(Mac::new([6, 5, 4, 3, 2, 1])).unwrap();
        let ll_addr: Ipv6Addr = mac.to_ipv6_link_local().addr().get();
        let multicast_addr = ll_addr.to_solicited_node_address();
        let local = DummyEventDispatcherBuilder::default();
        let remote = DummyEventDispatcherBuilder::default();
        let device_id = DeviceId::new_ethernet(0);

        let mut stack_builder = StackStateBuilder::default();
        let mut ndp_config = NdpConfiguration::default();
        ndp_config.set_max_router_solicitations(None);
        stack_builder.device_builder().set_default_ndp_config(ndp_config);

        // We explicitly call `build_with` when building our contexts below
        // because `build` will set the default NDP parameter
        // DUP_ADDR_DETECT_TRANSMITS to 0 (effectively disabling DAD) so we use
        // our own custom `StackStateBuilder` to set it to the default value of
        // `1` (see `DUP_ADDR_DETECT_TRANSMITS`).
        let mut net = DummyNetwork::new(
            vec![
                ("local", local.build_with(stack_builder.clone(), DummyEventDispatcher::default())),
                ("remote", remote.build_with(stack_builder, DummyEventDispatcher::default())),
            ]
            .into_iter(),
            |ctx, _| {
                if *ctx == "local" {
                    vec![("remote", device_id, None)]
                } else {
                    vec![("local", device_id, None)]
                }
            },
        );

        // Create the devices (will start DAD at the same time).
        assert_eq!(
            net.context("local").state.add_ethernet_device(mac, Ipv6::MINIMUM_LINK_MTU.into()),
            device_id
        );
        crate::device::initialize_device(net.context("local"), device_id);
        assert_eq!(
            net.context("remote").state.add_ethernet_device(mac, Ipv6::MINIMUM_LINK_MTU.into()),
            device_id
        );
        crate::device::initialize_device(net.context("remote"), device_id);
        assert_eq!(net.context("local").dispatcher.frames_sent().len(), 1);
        assert_eq!(net.context("remote").dispatcher.frames_sent().len(), 1);

        // Both devices should be in the solicited-node multicast group.
        assert!(get_ipv6_device_state(net.context("local"), device_id)
            .multicast_groups
            .contains(&multicast_addr));
        assert!(get_ipv6_device_state(net.context("remote"), device_id)
            .multicast_groups
            .contains(&multicast_addr));

        let _: StepResult = net.step();

        // They should now realize the address they intend to use has a
        // duplicate in the local network.
        assert_empty(get_assigned_ip_addr_subnets::<_, Ipv6Addr>(net.context("local"), device_id));
        assert_empty(get_assigned_ip_addr_subnets::<_, Ipv6Addr>(net.context("remote"), device_id));

        // Both devices should not be in the multicast group
        assert!(!get_ipv6_device_state(net.context("local"), device_id)
            .multicast_groups
            .contains(&multicast_addr));
        assert!(!get_ipv6_device_state(net.context("remote"), device_id)
            .multicast_groups
            .contains(&multicast_addr));
    }

    fn dad_timer_id(id: EthernetDeviceId, addr: UnicastAddr<Ipv6Addr>) -> TimerId {
        // We assume Ethernet since that's what all of our tests use.
        TimerId(TimerIdInner::DeviceLayer(DeviceLayerTimerId(DeviceLayerTimerIdInner::Ethernet(
            EthernetTimerId::Dad(crate::device::DadTimerId {
                device_id: id,
                addr,
                _marker: PhantomData,
            }),
        ))))
    }

    #[test]
    fn test_dad_duplicate_address_detected_advertisement() {
        // Tests whether a duplicate address will get detected by advertisement
        // In this test, one of the node first assigned itself the local_ip(),
        // then the second node comes up and it should be able to find out that
        // it cannot use the address because someone else has already taken that
        // address.
        set_logger_for_test();
        let mut local = DummyEventDispatcherBuilder::default();
        assert_eq!(local.add_device(local_mac()), 0);
        let mut remote = DummyEventDispatcherBuilder::default();
        assert_eq!(remote.add_device(remote_mac()), 0);
        let device_id = DeviceId::new_ethernet(0);

        let mut net = DummyNetwork::new(
            vec![
                ("local", local.build::<DummyEventDispatcher>()),
                ("remote", remote.build::<DummyEventDispatcher>()),
            ]
            .into_iter(),
            |ctx, _| {
                if *ctx == "local" {
                    vec![("remote", device_id, None)]
                } else {
                    vec![("local", device_id, None)]
                }
            },
        );

        // Enable DAD.
        let ipv6_config = crate::device::Ipv6DeviceConfiguration::default();
        crate::device::set_ipv6_configuration(net.context("local"), device_id, ipv6_config.clone());
        crate::device::set_ipv6_configuration(net.context("remote"), device_id, ipv6_config);

        let addr = AddrSubnet::new(local_ip().get(), 128).unwrap();
        let multicast_addr = local_ip().to_solicited_node_address();
        add_ip_addr_subnet(net.context("local"), device_id, addr).unwrap();
        // Only local should be in the solicited node multicast group.
        assert!(get_ipv6_device_state(net.context("local"), device_id)
            .multicast_groups
            .contains(&multicast_addr));
        assert!(!get_ipv6_device_state(net.context("remote"), device_id)
            .multicast_groups
            .contains(&multicast_addr));

        assert_eq!(
            testutil::trigger_next_timer(net.context("local")).unwrap(),
            dad_timer_id(device_id.try_into().expect("expected ethernet ID"), local_ip())
        );

        assert!(NdpContext::<EthernetLinkDevice>::get_ip_device_state(
            net.context("local"),
            device_id.try_into().expect("expected ethernet ID")
        )
        .find_addr(&local_ip())
        .unwrap()
        .state
        .is_assigned());

        add_ip_addr_subnet(net.context("remote"), device_id, addr).unwrap();
        // Local & remote should be in the multicast group.
        assert!(get_ipv6_device_state(net.context("local"), device_id)
            .multicast_groups
            .contains(&multicast_addr));
        assert!(get_ipv6_device_state(net.context("remote"), device_id)
            .multicast_groups
            .contains(&multicast_addr));

        let _: StepResult = net.step();

        assert_eq!(
            get_assigned_ip_addr_subnets::<_, Ipv6Addr>(net.context("remote"), device_id).count(),
            1
        );
        // Let's make sure that our local node still can use that address.
        assert!(NdpContext::<EthernetLinkDevice>::get_ip_device_state(
            net.context("local"),
            device_id.try_into().expect("expected ethernet ID")
        )
        .find_addr(&local_ip())
        .unwrap()
        .state
        .is_assigned());

        // Only local should be in the solicited node multicast group.
        assert!(get_ipv6_device_state(net.context("local"), device_id)
            .multicast_groups
            .contains(&multicast_addr));
        assert!(!get_ipv6_device_state(net.context("remote"), device_id)
            .multicast_groups
            .contains(&multicast_addr));
    }

    #[test]
    fn test_dad_set_ipv6_address_when_ongoing() {
        // Test that we can make our tentative address change when DAD is
        // ongoing.

        // We explicitly call `build_with` when building our context below
        // because `build` will set the default NDP parameter
        // DUP_ADDR_DETECT_TRANSMITS to 0 (effectively disabling DAD) so we use
        // our own custom `StackStateBuilder` to set it to the default value of
        // `1` (see `DUP_ADDR_DETECT_TRANSMITS`).
        let mut ctx = DummyEventDispatcherBuilder::default()
            .build_with(StackStateBuilder::default(), DummyEventDispatcher::default());
        let dev_id = ctx.state.add_ethernet_device(local_mac(), Ipv6::MINIMUM_LINK_MTU.into());
        crate::device::initialize_device(&mut ctx, dev_id);
        let addr = local_ip();
        add_ip_addr_subnet(&mut ctx, dev_id, AddrSubnet::new(addr.get(), 128).unwrap()).unwrap();
        assert_eq!(
            NdpContext::<EthernetLinkDevice>::get_ip_device_state(
                &ctx,
                dev_id.try_into().expect("expected ethernet ID")
            )
            .find_addr(&addr)
            .unwrap()
            .state,
            AddressState::Tentative { dad_transmits_remaining: None },
        );
        let addr = remote_ip();
        assert_eq!(
            NdpContext::<EthernetLinkDevice>::get_ip_device_state(
                &ctx,
                dev_id.try_into().expect("expected ethernet ID")
            )
            .find_addr(&addr),
            None
        );
        add_ip_addr_subnet(&mut ctx, dev_id, AddrSubnet::new(addr.get(), 128).unwrap()).unwrap();
        assert_eq!(
            NdpContext::<EthernetLinkDevice>::get_ip_device_state(
                &ctx,
                dev_id.try_into().expect("expected ethernet ID")
            )
            .find_addr(&addr)
            .unwrap()
            .state,
            AddressState::Tentative { dad_transmits_remaining: None },
        );
    }

    #[test]
    fn test_dad_three_transmits_no_conflicts() {
        let mut stack_builder = StackStateBuilder::default();
        let mut ndp_config = crate::device::ndp::NdpConfiguration::default();
        ndp_config.set_max_router_solicitations(None);
        stack_builder.device_builder().set_default_ndp_config(ndp_config);
        let mut ipv6_config = crate::device::Ipv6DeviceConfiguration::default();
        ipv6_config.set_dad_transmits(None);
        stack_builder.device_builder().set_default_ipv6_config(ipv6_config);
        let mut ctx = Ctx::new(stack_builder.build(), DummyEventDispatcher::default());
        let dev_id = ctx.state.add_ethernet_device(local_mac(), Ipv6::MINIMUM_LINK_MTU.into());
        crate::device::initialize_device(&mut ctx, dev_id);

        // Enable DAD.
        let mut ipv6_config = crate::device::Ipv6DeviceConfiguration::default();
        ipv6_config.set_dad_transmits(NonZeroU8::new(3));
        crate::device::set_ipv6_configuration(&mut ctx, dev_id, ipv6_config);
        add_ip_addr_subnet(&mut ctx, dev_id, AddrSubnet::new(local_ip().get(), 128).unwrap())
            .unwrap();
        for _ in 0..3 {
            assert_eq!(
                testutil::trigger_next_timer(&mut ctx).unwrap(),
                dad_timer_id(dev_id.try_into().expect("expected ethernet ID"), local_ip())
            );
        }
        assert!(NdpContext::<EthernetLinkDevice>::get_ip_device_state(
            &ctx,
            dev_id.try_into().expect("expected ethernet ID")
        )
        .find_addr(&local_ip())
        .unwrap()
        .state
        .is_assigned());
    }

    #[test]
    fn test_dad_three_transmits_with_conflicts() {
        // Test if the implementation is correct when we have more than 1 NS
        // packets to send.
        set_logger_for_test();
        let mac = UnicastAddr::new(Mac::new([6, 5, 4, 3, 2, 1])).unwrap();
        let mut local = DummyEventDispatcherBuilder::default();
        assert_eq!(local.add_device(mac), 0);
        let mut remote = DummyEventDispatcherBuilder::default();
        assert_eq!(remote.add_device(mac), 0);
        let device_id = DeviceId::new_ethernet(0);
        let mut net = DummyNetwork::new(
            vec![("local", local.build()), ("remote", remote.build())].into_iter(),
            |ctx, _| {
                if *ctx == "local" {
                    vec![("remote", device_id, None)]
                } else {
                    vec![("local", device_id, None)]
                }
            },
        );

        let mut ipv6_config = crate::device::Ipv6DeviceConfiguration::default();
        ipv6_config.set_dad_transmits(NonZeroU8::new(3));
        crate::device::set_ipv6_configuration(net.context("local"), device_id, ipv6_config.clone());
        crate::device::set_ipv6_configuration(net.context("remote"), device_id, ipv6_config);

        add_ip_addr_subnet(
            net.context("local"),
            device_id,
            AddrSubnet::new(local_ip().get(), 128).unwrap(),
        )
        .unwrap();

        let expected_timer_id =
            dad_timer_id(device_id.try_into().expect("expected ethernet ID"), local_ip());
        // During the first and second period, the remote host is still down.
        assert_eq!(testutil::trigger_next_timer(net.context("local")).unwrap(), expected_timer_id);
        assert_eq!(testutil::trigger_next_timer(net.context("local")).unwrap(), expected_timer_id);
        add_ip_addr_subnet(
            net.context("remote"),
            device_id,
            AddrSubnet::new(local_ip().get(), 128).unwrap(),
        )
        .unwrap();
        // The local host should have sent out 3 packets while the remote one
        // should only have sent out 1.
        assert_eq!(net.context("local").dispatcher.frames_sent().len(), 3);
        assert_eq!(net.context("remote").dispatcher.frames_sent().len(), 1);

        let _: StepResult = net.step();

        // Let's make sure that all timers are cancelled properly.
        assert_empty(net.context("local").dispatcher.timer_events());
        assert_empty(net.context("remote").dispatcher.timer_events());

        // They should now realize the address they intend to use has a
        // duplicate in the local network.
        assert_eq!(
            get_assigned_ip_addr_subnets::<_, Ipv6Addr>(net.context("local"), device_id).count(),
            1
        );
        assert_eq!(
            get_assigned_ip_addr_subnets::<_, Ipv6Addr>(net.context("remote"), device_id).count(),
            1
        );
    }

    #[test]
    fn test_dad_multiple_ips_simultaneously() {
        let mut ctx = DummyEventDispatcherBuilder::default().build::<DummyEventDispatcher>();
        let dev_id = ctx.state.add_ethernet_device(local_mac(), Ipv6::MINIMUM_LINK_MTU.into());
        crate::device::initialize_device(&mut ctx, dev_id);

        assert_empty(ctx.dispatcher.frames_sent());

        let mut ndp_config = NdpConfiguration::default();
        ndp_config.set_max_router_solicitations(None);
        crate::device::set_ndp_configuration(&mut ctx, dev_id, ndp_config)
            .expect("error setting NDP configuuration");
        let mut ipv6_config = crate::device::Ipv6DeviceConfiguration::default();
        ipv6_config.set_dad_transmits(NonZeroU8::new(3));
        crate::device::set_ipv6_configuration(&mut ctx, dev_id, ipv6_config);

        // Add an IP.
        add_ip_addr_subnet(&mut ctx, dev_id, AddrSubnet::new(local_ip().get(), 128).unwrap())
            .unwrap();
        assert!(get_ipv6_device_state(&ctx, dev_id)
            .find_addr(&local_ip())
            .unwrap()
            .state
            .is_tentative());
        assert_eq!(ctx.dispatcher.frames_sent().len(), 1);

        // Send another NS.
        let local_timer_id =
            dad_timer_id(dev_id.try_into().expect("expected ethernet ID"), local_ip());
        assert_eq!(run_for(&mut ctx, Duration::from_secs(1)), [local_timer_id]);
        assert_eq!(ctx.dispatcher.frames_sent().len(), 2);

        // Add another IP
        add_ip_addr_subnet(&mut ctx, dev_id, AddrSubnet::new(remote_ip().get(), 128).unwrap())
            .unwrap();
        assert!(get_ipv6_device_state(&ctx, dev_id)
            .find_addr(&local_ip())
            .unwrap()
            .state
            .is_tentative());
        assert!(get_ipv6_device_state(&ctx, dev_id)
            .find_addr(&remote_ip())
            .unwrap()
            .state
            .is_tentative());
        assert_eq!(ctx.dispatcher.frames_sent().len(), 3);

        // Run to the end for DAD for local ip
        let remote_timer_id =
            dad_timer_id(dev_id.try_into().expect("expected ethernet ID"), remote_ip());
        assert_eq!(
            run_for(&mut ctx, Duration::from_secs(2)),
            [local_timer_id, remote_timer_id, local_timer_id, remote_timer_id]
        );
        assert!(get_ipv6_device_state(&ctx, dev_id)
            .find_addr(&local_ip())
            .unwrap()
            .state
            .is_assigned());
        assert!(get_ipv6_device_state(&ctx, dev_id)
            .find_addr(&remote_ip())
            .unwrap()
            .state
            .is_tentative());
        assert_eq!(ctx.dispatcher.frames_sent().len(), 6);

        // Run to the end for DAD for local ip
        assert_eq!(run_for(&mut ctx, Duration::from_secs(1)), [remote_timer_id]);
        assert!(get_ipv6_device_state(&ctx, dev_id)
            .find_addr(&local_ip())
            .unwrap()
            .state
            .is_assigned());
        assert!(get_ipv6_device_state(&ctx, dev_id)
            .find_addr(&remote_ip())
            .unwrap()
            .state
            .is_assigned());
        assert_eq!(ctx.dispatcher.frames_sent().len(), 6);

        // No more timers.
        assert_eq!(trigger_next_timer(&mut ctx), None);
    }

    #[test]
    fn test_dad_cancel_when_ip_removed() {
        let mut ctx = DummyEventDispatcherBuilder::default().build::<DummyEventDispatcher>();
        let dev_id = ctx.state.add_ethernet_device(local_mac(), Ipv6::MINIMUM_LINK_MTU.into());
        crate::device::initialize_device(&mut ctx, dev_id);

        // Enable DAD.
        let mut ndp_config = NdpConfiguration::default();
        ndp_config.set_max_router_solicitations(None);
        crate::device::set_ndp_configuration(&mut ctx, dev_id, ndp_config)
            .expect("error setting NDP configuuration");
        let mut ipv6_config = crate::device::Ipv6DeviceConfiguration::default();
        ipv6_config.set_dad_transmits(NonZeroU8::new(3));
        crate::device::set_ipv6_configuration(&mut ctx, dev_id, ipv6_config);

        assert_empty(ctx.dispatcher.frames_sent());

        // Add an IP.
        add_ip_addr_subnet(&mut ctx, dev_id, AddrSubnet::new(local_ip().get(), 128).unwrap())
            .unwrap();
        assert!(get_ipv6_device_state(&ctx, dev_id)
            .find_addr(&local_ip())
            .unwrap()
            .state
            .is_tentative());
        assert_eq!(ctx.dispatcher.frames_sent().len(), 1);

        // Send another NS.
        let local_timer_id =
            dad_timer_id(dev_id.try_into().expect("expected ethernet ID"), local_ip());
        assert_eq!(run_for(&mut ctx, Duration::from_secs(1)), [local_timer_id]);
        assert_eq!(ctx.dispatcher.frames_sent().len(), 2);

        // Add another IP
        add_ip_addr_subnet(&mut ctx, dev_id, AddrSubnet::new(remote_ip().get(), 128).unwrap())
            .unwrap();
        assert!(get_ipv6_device_state(&ctx, dev_id)
            .find_addr(&local_ip())
            .unwrap()
            .state
            .is_tentative());
        assert!(get_ipv6_device_state(&ctx, dev_id)
            .find_addr(&remote_ip())
            .unwrap()
            .state
            .is_tentative());
        assert_eq!(ctx.dispatcher.frames_sent().len(), 3);

        // Run 1s
        let remote_timer_id =
            dad_timer_id(dev_id.try_into().expect("expected ethernet ID"), remote_ip());
        assert_eq!(run_for(&mut ctx, Duration::from_secs(1)), [local_timer_id, remote_timer_id]);
        assert!(get_ipv6_device_state(&ctx, dev_id)
            .find_addr(&local_ip())
            .unwrap()
            .state
            .is_tentative());
        assert!(get_ipv6_device_state(&ctx, dev_id)
            .find_addr(&remote_ip())
            .unwrap()
            .state
            .is_tentative());
        assert_eq!(ctx.dispatcher.frames_sent().len(), 5);

        // Remove local ip
        del_ip_addr(&mut ctx, dev_id, &local_ip().into_specified()).unwrap();
        assert_eq!(get_ipv6_device_state(&ctx, dev_id).find_addr(&local_ip()), None);
        assert!(get_ipv6_device_state(&ctx, dev_id)
            .find_addr(&remote_ip())
            .unwrap()
            .state
            .is_tentative());
        assert_eq!(ctx.dispatcher.frames_sent().len(), 5);

        // Run to the end for DAD for local ip
        assert_eq!(run_for(&mut ctx, Duration::from_secs(2)), [remote_timer_id, remote_timer_id]);
        assert_eq!(get_ipv6_device_state(&ctx, dev_id).find_addr(&local_ip()), None);
        assert!(get_ipv6_device_state(&ctx, dev_id)
            .find_addr(&remote_ip())
            .unwrap()
            .state
            .is_assigned());
        assert_eq!(ctx.dispatcher.frames_sent().len(), 6);

        // No more timers.
        assert_eq!(trigger_next_timer(&mut ctx), None);
    }

    trait UnwrapNdp<B: ByteSlice> {
        fn unwrap_ndp(self) -> NdpPacket<B>;
    }

    impl<B: ByteSlice> UnwrapNdp<B> for Icmpv6Packet<B> {
        fn unwrap_ndp(self) -> NdpPacket<B> {
            match self {
                Icmpv6Packet::Ndp(ndp) => ndp,
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn test_receiving_router_solicitation_validity_check() {
        let config = Ipv6::DUMMY_CONFIG;
        let src_ip = Ipv6Addr::from([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 192, 168, 0, 10]);
        let src_mac = [10, 11, 12, 13, 14, 15];
        let options = vec![NdpOptionBuilder::SourceLinkLayerAddress(&src_mac[..])];

        // Test receiving NDP RS when not a router (should not receive)

        let mut ctx = DummyEventDispatcherBuilder::from_config(config.clone())
            .build::<DummyEventDispatcher>();
        let device_id = DeviceId::new_ethernet(0);

        let mut icmpv6_packet_buf = OptionSequenceBuilder::new(options.iter())
            .into_serializer()
            .encapsulate(IcmpPacketBuilder::<Ipv6, &[u8], _>::new(
                src_ip,
                config.local_ip,
                IcmpUnusedCode,
                RouterSolicitation::default(),
            ))
            .serialize_vec_outer()
            .unwrap();
        let icmpv6_packet = icmpv6_packet_buf
            .parse_with::<_, Icmpv6Packet<_>>(IcmpParseArgs::new(src_ip, config.local_ip))
            .unwrap();

        ctx.receive_ndp_packet(
            device_id,
            src_ip.try_into().unwrap(),
            config.local_ip,
            icmpv6_packet.unwrap_ndp(),
        );
        assert_eq!(get_counter_val(&mut ctx, "ndp::rx_router_solicitation"), 0);
    }

    #[test]
    fn test_receiving_router_advertisement_validity_check() {
        let config = Ipv6::DUMMY_CONFIG;
        let src_mac = [10, 11, 12, 13, 14, 15];
        let src_ip = Ipv6Addr::from([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 192, 168, 0, 10]);
        let mut ctx = DummyEventDispatcherBuilder::from_config(config.clone())
            .build::<DummyEventDispatcher>();
        let device_id = DeviceId::new_ethernet(0);

        // Test receiving NDP RA where source IP is not a link local address
        // (should not receive).

        let mut icmpv6_packet_buf = router_advertisement_message(
            src_ip.into(),
            config.local_ip.get(),
            1,
            false,
            false,
            3,
            4,
            5,
        );
        let icmpv6_packet = icmpv6_packet_buf
            .parse_with::<_, Icmpv6Packet<_>>(IcmpParseArgs::new(src_ip, config.local_ip))
            .unwrap();
        ctx.receive_ndp_packet(
            device_id,
            src_ip.try_into().unwrap(),
            config.local_ip,
            icmpv6_packet.unwrap_ndp(),
        );
        assert_eq!(get_counter_val(&mut ctx, "ndp::rx_router_advertisement"), 0);

        // Test receiving NDP RA where source IP is a link local address (should
        // receive).

        let src_ip = Mac::new(src_mac).to_ipv6_link_local().addr().get();
        let mut icmpv6_packet_buf =
            router_advertisement_message(src_ip, config.local_ip.get(), 1, false, false, 3, 4, 5);
        let icmpv6_packet = icmpv6_packet_buf
            .parse_with::<_, Icmpv6Packet<_>>(IcmpParseArgs::new(src_ip, config.local_ip))
            .unwrap();
        ctx.receive_ndp_packet(
            device_id,
            src_ip.try_into().unwrap(),
            config.local_ip,
            icmpv6_packet.unwrap_ndp(),
        );
        assert_eq!(get_counter_val(&mut ctx, "ndp::rx_router_advertisement"), 1);
    }

    #[test]
    fn test_receiving_router_advertisement_fixed_message() {
        let config = Ipv6::DUMMY_CONFIG;
        let mut ctx = DummyEventDispatcherBuilder::from_config(config.clone())
            .build::<DummyEventDispatcher>();
        let device_id = DeviceId::new_ethernet(0);
        let src_ip = config.remote_mac.to_ipv6_link_local().addr();

        // Receive a router advertisement for a brand new router with a valid
        // lifetime.

        let mut icmpv6_packet_buf = router_advertisement_message(
            src_ip.get(),
            config.local_ip.get(),
            1,
            false,
            false,
            3,
            4,
            5,
        );
        let icmpv6_packet = icmpv6_packet_buf
            .parse_with::<_, Icmpv6Packet<_>>(IcmpParseArgs::new(src_ip, config.local_ip))
            .unwrap();
        assert!(!StateContext::<NdpState<EthernetLinkDevice>, _>::get_state_mut_with(
            &mut ctx,
            device_id.try_into().expect("expected ethernet ID"),
        )
        .has_default_router(&src_ip));
        ctx.receive_ndp_packet(
            device_id,
            Ipv6SourceAddr::from_witness(src_ip).unwrap(),
            config.local_ip,
            icmpv6_packet.unwrap_ndp(),
        );
        assert_eq!(get_counter_val(&mut ctx, "ndp::rx_router_advertisement"), 1);
        let ndp_state = StateContext::<NdpState<EthernetLinkDevice>, _>::get_state_mut_with(
            &mut ctx,
            device_id.try_into().expect("expected ethernet ID"),
        );
        // We should have the new router in our list with our NDP parameters
        // updated.
        assert!(ndp_state.has_default_router(&src_ip));
        let base = Duration::from_millis(4);
        let min_reachable = base / 2;
        let max_reachable = min_reachable * 3;
        assert_eq!(ndp_state.base_reachable_time, base);
        assert!(
            ndp_state.reachable_time >= min_reachable && ndp_state.reachable_time <= max_reachable
        );
        assert_eq!(ndp_state.retrans_timer, Duration::from_millis(5));
        assert_eq!(get_ipv6_hop_limit(&ctx, device_id).get(), 1);

        // Receive a new router advertisement for the same router with a valid
        // lifetime.

        let mut icmpv6_packet_buf = router_advertisement_message(
            src_ip.get(),
            config.local_ip.get(),
            7,
            false,
            false,
            9,
            10,
            11,
        );
        let icmpv6_packet = icmpv6_packet_buf
            .parse_with::<_, Icmpv6Packet<_>>(IcmpParseArgs::new(src_ip, config.local_ip))
            .unwrap();
        ctx.receive_ndp_packet(
            device_id,
            Ipv6SourceAddr::from_witness(src_ip).unwrap(),
            config.local_ip,
            icmpv6_packet.unwrap_ndp(),
        );
        assert_eq!(get_counter_val(&mut ctx, "ndp::rx_router_advertisement"), 2);
        let ndp_state = StateContext::<NdpState<EthernetLinkDevice>, _>::get_state_mut_with(
            &mut ctx,
            device_id.try_into().expect("expected ethernet ID"),
        );
        assert!(ndp_state.has_default_router(&src_ip));
        let base = Duration::from_millis(10);
        let min_reachable = base / 2;
        let max_reachable = min_reachable * 3;
        let reachable_time = ndp_state.reachable_time;
        assert_eq!(ndp_state.base_reachable_time, base);
        assert!(
            ndp_state.reachable_time >= min_reachable && ndp_state.reachable_time <= max_reachable
        );
        assert_eq!(ndp_state.retrans_timer, Duration::from_millis(11));
        assert_eq!(get_ipv6_hop_limit(&ctx, device_id).get(), 7);

        // Receive a new router advertisement for the same router with a valid
        // lifetime and zero valued parameters.

        // Zero value for Reachable Time should not update base_reachable_time.
        // Other non zero values should update.
        let mut icmpv6_packet_buf = router_advertisement_message(
            src_ip.get(),
            config.local_ip.get(),
            13,
            false,
            false,
            15,
            0,
            17,
        );
        let icmpv6_packet = icmpv6_packet_buf
            .parse_with::<_, Icmpv6Packet<_>>(IcmpParseArgs::new(src_ip, config.local_ip))
            .unwrap();
        ctx.receive_ndp_packet(
            device_id,
            Ipv6SourceAddr::from_witness(src_ip).unwrap(),
            config.local_ip,
            icmpv6_packet.unwrap_ndp(),
        );
        assert_eq!(get_counter_val(&mut ctx, "ndp::rx_router_advertisement"), 3);
        let ndp_state = StateContext::<NdpState<EthernetLinkDevice>, _>::get_state_mut_with(
            &mut ctx,
            device_id.try_into().expect("expected ethernet ID"),
        );
        assert!(ndp_state.has_default_router(&src_ip));
        // Should be the same value as before.
        assert_eq!(ndp_state.base_reachable_time, base);
        // Should be the same randomly calculated value as before.
        assert_eq!(ndp_state.reachable_time, reachable_time);
        // Should update to new value.
        assert_eq!(ndp_state.retrans_timer, Duration::from_millis(17));
        // Should update to new value.
        assert_eq!(get_ipv6_hop_limit(&ctx, device_id).get(), 13);

        // Zero value for Retransmit Time should not update our retrans_time.
        // Other non zero values should update.
        let mut icmpv6_packet_buf = router_advertisement_message(
            src_ip.get(),
            config.local_ip.get(),
            19,
            false,
            false,
            21,
            22,
            0,
        );
        let icmpv6_packet = icmpv6_packet_buf
            .parse_with::<_, Icmpv6Packet<_>>(IcmpParseArgs::new(src_ip, config.local_ip))
            .unwrap();
        ctx.receive_ndp_packet(
            device_id,
            Ipv6SourceAddr::from_witness(src_ip).unwrap(),
            config.local_ip,
            icmpv6_packet.unwrap_ndp(),
        );
        assert_eq!(get_counter_val(&mut ctx, "ndp::rx_router_advertisement"), 4);
        let ndp_state = StateContext::<NdpState<EthernetLinkDevice>, _>::get_state_mut_with(
            &mut ctx,
            device_id.try_into().expect("expected ethernet ID"),
        );
        assert!(ndp_state.has_default_router(&src_ip));
        // Should update to new value.
        let base = Duration::from_millis(22);
        let min_reachable = base / 2;
        let max_reachable = min_reachable * 3;
        assert_eq!(ndp_state.base_reachable_time, base);
        assert!(
            ndp_state.reachable_time >= min_reachable && ndp_state.reachable_time <= max_reachable
        );
        // Should be the same value as before.
        assert_eq!(ndp_state.retrans_timer, Duration::from_millis(17));
        // Should update to new value.
        assert_eq!(get_ipv6_hop_limit(&ctx, device_id).get(), 19);

        // Zero value for CurrHopLimit should not update our hop_limit. Other
        // non zero values should update.
        let mut icmpv6_packet_buf = router_advertisement_message(
            src_ip.get(),
            config.local_ip.get(),
            0,
            false,
            false,
            27,
            28,
            29,
        );
        let icmpv6_packet = icmpv6_packet_buf
            .parse_with::<_, Icmpv6Packet<_>>(IcmpParseArgs::new(src_ip, config.local_ip))
            .unwrap();
        ctx.receive_ndp_packet(
            device_id,
            Ipv6SourceAddr::from_witness(src_ip).unwrap(),
            config.local_ip,
            icmpv6_packet.unwrap_ndp(),
        );
        assert_eq!(get_counter_val(&mut ctx, "ndp::rx_router_advertisement"), 5);
        let ndp_state = StateContext::<NdpState<EthernetLinkDevice>, _>::get_state_mut_with(
            &mut ctx,
            device_id.try_into().expect("expected ethernet ID"),
        );
        assert!(ndp_state.has_default_router(&src_ip));
        // Should update to new value.
        let base = Duration::from_millis(28);
        let min_reachable = base / 2;
        let max_reachable = min_reachable * 3;
        assert_eq!(ndp_state.base_reachable_time, base);
        assert!(
            ndp_state.reachable_time >= min_reachable && ndp_state.reachable_time <= max_reachable
        );
        // Should update to new value.
        assert_eq!(ndp_state.retrans_timer, Duration::from_millis(29));
        // Should be the same value as before.
        assert_eq!(get_ipv6_hop_limit(&ctx, device_id).get(), 19);

        // Receive new router advertisement with 0 router lifetime, but new
        // parameters.

        let mut icmpv6_packet_buf = router_advertisement_message(
            src_ip.get(),
            config.local_ip.get(),
            31,
            false,
            false,
            0,
            34,
            35,
        );
        let icmpv6_packet = icmpv6_packet_buf
            .parse_with::<_, Icmpv6Packet<_>>(IcmpParseArgs::new(src_ip, config.local_ip))
            .unwrap();
        ctx.receive_ndp_packet(
            device_id,
            Ipv6SourceAddr::from_witness(src_ip).unwrap(),
            config.local_ip,
            icmpv6_packet.unwrap_ndp(),
        );
        assert_eq!(get_counter_val(&mut ctx, "ndp::rx_router_advertisement"), 6);
        let ndp_state = StateContext::<NdpState<EthernetLinkDevice>, _>::get_state_mut_with(
            &mut ctx,
            device_id.try_into().expect("expected ethernet ID"),
        );
        // Router should no longer be in our list.
        assert!(!ndp_state.has_default_router(&src_ip));
        let base = Duration::from_millis(34);
        let min_reachable = base / 2;
        let max_reachable = min_reachable * 3;
        assert_eq!(ndp_state.base_reachable_time, base);
        assert!(
            ndp_state.reachable_time >= min_reachable && ndp_state.reachable_time <= max_reachable
        );
        assert_eq!(ndp_state.retrans_timer, Duration::from_millis(35));
        assert_eq!(get_ipv6_hop_limit(&ctx, device_id).get(), 31);

        // Router invalidation timeout must have been cleared since we invalided
        // with the received router advertisement with lifetime 0.
        assert_eq!(trigger_next_timer(&mut ctx), None);

        // Receive new router advertisement with non-0 router lifetime, but let
        // it get invalidated.

        let mut icmpv6_packet_buf = router_advertisement_message(
            src_ip.get(),
            config.local_ip.get(),
            37,
            false,
            false,
            39,
            40,
            41,
        );
        let icmpv6_packet = icmpv6_packet_buf
            .parse_with::<_, Icmpv6Packet<_>>(IcmpParseArgs::new(src_ip, config.local_ip))
            .unwrap();
        ctx.receive_ndp_packet(
            device_id,
            Ipv6SourceAddr::from_witness(src_ip).unwrap(),
            config.local_ip,
            icmpv6_packet.unwrap_ndp(),
        );
        assert_eq!(get_counter_val(&mut ctx, "ndp::rx_router_advertisement"), 7);
        let ndp_state = StateContext::<NdpState<EthernetLinkDevice>, _>::get_state_mut_with(
            &mut ctx,
            device_id.try_into().expect("expected ethernet ID"),
        );
        // Router should be re-added.
        assert!(ndp_state.has_default_router(&src_ip));
        let base = Duration::from_millis(40);
        let min_reachable = base / 2;
        let max_reachable = min_reachable * 3;
        assert_eq!(ndp_state.base_reachable_time, base);
        assert!(
            ndp_state.reachable_time >= min_reachable && ndp_state.reachable_time <= max_reachable
        );
        assert_eq!(ndp_state.retrans_timer, Duration::from_millis(41));
        assert_eq!(get_ipv6_hop_limit(&ctx, device_id).get(), 37);

        // Invalidate the router by triggering the timeout.
        assert_eq!(
            trigger_next_timer(&mut ctx).unwrap(),
            NdpTimerId::new_router_invalidation(
                device_id.try_into().expect("expected ethernet ID"),
                src_ip
            )
            .into()
        );
        let ndp_state = StateContext::<NdpState<EthernetLinkDevice>, _>::get_state_mut_with(
            &mut ctx,
            device_id.try_into().expect("expected ethernet ID"),
        );
        assert!(!ndp_state.has_default_router(&src_ip));

        // No more timers.
        assert_eq!(trigger_next_timer(&mut ctx), None);
    }

    #[test]
    fn test_sending_ipv6_packet_after_hop_limit_change() {
        // Sets the hop limit with a router advertisement and sends a packet to
        // make sure the packet uses the new hop limit.
        fn inner_test(ctx: &mut Ctx<DummyEventDispatcher>, hop_limit: u8, frame_offset: usize) {
            let config = Ipv6::DUMMY_CONFIG;
            let device_id = DeviceId::new_ethernet(0);
            let src_ip = config.remote_mac.to_ipv6_link_local().addr();

            let mut icmpv6_packet_buf = router_advertisement_message(
                src_ip.get(),
                config.local_ip.get(),
                hop_limit,
                false,
                false,
                0,
                0,
                0,
            );
            let icmpv6_packet = icmpv6_packet_buf
                .parse_with::<_, Icmpv6Packet<_>>(IcmpParseArgs::new(src_ip, config.local_ip))
                .unwrap();
            assert!(!StateContext::<NdpState<EthernetLinkDevice>, _>::get_state_mut_with(
                ctx,
                device_id.try_into().expect("expected ethernet ID")
            )
            .has_default_router(&src_ip));
            ctx.receive_ndp_packet(
                device_id,
                Ipv6SourceAddr::from_witness(src_ip).unwrap(),
                config.local_ip,
                icmpv6_packet.unwrap_ndp(),
            );
            assert_eq!(get_ipv6_hop_limit(ctx, device_id).get(), hop_limit);
            crate::ip::send_ip_packet_from_device(
                ctx,
                device_id,
                config.local_ip.get(),
                config.remote_ip.get(),
                config.remote_ip,
                IpProto::Tcp.into(),
                Buf::new(vec![0; 10], ..),
                None,
            )
            .unwrap();
            let (buf, _, _, _) =
                parse_ethernet_frame(&ctx.dispatcher.frames_sent()[frame_offset].1[..]).unwrap();
            // Packet's hop limit should be 100.
            assert_eq!(buf[7], hop_limit);
        }

        let mut ctx = DummyEventDispatcherBuilder::from_config(Ipv6::DUMMY_CONFIG)
            .build::<DummyEventDispatcher>();

        // Set hop limit to 100.
        inner_test(&mut ctx, 100, 0);

        // Set hop limit to 30.
        inner_test(&mut ctx, 30, 1);
    }

    #[test]
    fn test_receiving_router_advertisement_source_link_layer_option() {
        let config = Ipv6::DUMMY_CONFIG;
        let mut ctx = DummyEventDispatcherBuilder::from_config(config.clone())
            .build::<DummyEventDispatcher>();
        let device_id = DeviceId::new_ethernet(0);
        let src_mac = Mac::new([10, 11, 12, 13, 14, 15]);
        let src_ip = src_mac.to_ipv6_link_local().addr();
        let src_mac_bytes = src_mac.bytes();
        let options = vec![NdpOptionBuilder::SourceLinkLayerAddress(&src_mac_bytes[..])];

        // First receive a Router Advertisement without the source link layer
        // and make sure no new neighbor gets added.

        let mut icmpv6_packet_buf = router_advertisement_message(
            src_ip.get(),
            config.local_ip.get(),
            1,
            false,
            false,
            3,
            4,
            5,
        );
        let icmpv6_packet = icmpv6_packet_buf
            .parse_with::<_, Icmpv6Packet<_>>(IcmpParseArgs::new(src_ip, config.local_ip))
            .unwrap();
        let ndp_state = StateContext::<NdpState<EthernetLinkDevice>, _>::get_state_mut_with(
            &mut ctx,
            device_id.try_into().expect("expected ethernet ID"),
        );
        assert!(!ndp_state.has_default_router(&src_ip));
        assert_eq!(ndp_state.neighbors.get_neighbor_state(&src_ip), None);
        ctx.receive_ndp_packet(
            device_id,
            Ipv6SourceAddr::from_witness(src_ip).unwrap(),
            config.local_ip,
            icmpv6_packet.unwrap_ndp(),
        );
        assert_eq!(get_counter_val(&mut ctx, "ndp::rx_router_advertisement"), 1);
        let ndp_state = StateContext::<NdpState<EthernetLinkDevice>, _>::get_state_mut_with(
            &mut ctx,
            device_id.try_into().expect("expected ethernet ID"),
        );
        // We should have the new router in our list with our NDP parameters
        // updated.
        assert!(ndp_state.has_default_router(&src_ip));
        // Should still not have a neighbor added.
        assert_eq!(ndp_state.neighbors.get_neighbor_state(&src_ip), None);

        // Receive a new RA but with the source link layer option

        let mut icmpv6_packet_buf = OptionSequenceBuilder::new(options.iter())
            .into_serializer()
            .encapsulate(IcmpPacketBuilder::<Ipv6, &[u8], _>::new(
                src_ip,
                config.local_ip,
                IcmpUnusedCode,
                RouterAdvertisement::new(1, false, false, 3, 4, 5),
            ))
            .serialize_vec_outer()
            .unwrap();
        let icmpv6_packet = icmpv6_packet_buf
            .parse_with::<_, Icmpv6Packet<_>>(IcmpParseArgs::new(src_ip, config.local_ip))
            .unwrap();
        ctx.receive_ndp_packet(
            device_id,
            Ipv6SourceAddr::from_witness(src_ip).unwrap(),
            config.local_ip,
            icmpv6_packet.unwrap_ndp(),
        );
        assert_eq!(get_counter_val(&mut ctx, "ndp::rx_router_advertisement"), 2);
        let ndp_state = StateContext::<NdpState<EthernetLinkDevice>, _>::get_state_mut_with(
            &mut ctx,
            device_id.try_into().expect("expected ethernet ID"),
        );
        assert!(ndp_state.has_default_router(&src_ip));
        let neighbor = ndp_state.neighbors.get_neighbor_state(&src_ip).unwrap();
        assert_eq!(neighbor.link_address.unwrap(), src_mac);
        assert!(neighbor.is_router);
        // Router should be marked stale as a neighbor.
        assert_eq!(neighbor.state, NeighborEntryState::Stale);

        // Trigger router invalidation.
        assert_eq!(
            trigger_next_timer(&mut ctx).unwrap(),
            NdpTimerId::new_router_invalidation(
                device_id.try_into().expect("expected ethernet ID"),
                src_ip
            )
            .into()
        );

        // Neighbor entry shouldn't change except for `is_router` which should
        // now be `false`.
        let ndp_state = StateContext::<NdpState<EthernetLinkDevice>, _>::get_state_mut_with(
            &mut ctx,
            device_id.try_into().expect("expected ethernet ID"),
        );
        assert!(!ndp_state.has_default_router(&src_ip));
        let neighbor = ndp_state.neighbors.get_neighbor_state(&src_ip).unwrap();
        assert_eq!(neighbor.link_address.unwrap(), src_mac);
        assert!(!neighbor.is_router);
        assert_eq!(neighbor.state, NeighborEntryState::Stale);
    }

    #[test]
    fn test_receiving_router_advertisement_mtu_option() {
        fn packet_buf(src_ip: Ipv6Addr, dst_ip: Ipv6Addr, mtu: u32) -> Buf<Vec<u8>> {
            let options = &[NdpOptionBuilder::Mtu(mtu)];
            OptionSequenceBuilder::new(options.iter())
                .into_serializer()
                .encapsulate(IcmpPacketBuilder::<Ipv6, &[u8], _>::new(
                    src_ip,
                    dst_ip,
                    IcmpUnusedCode,
                    RouterAdvertisement::new(1, false, false, 3, 4, 5),
                ))
                .serialize_vec_outer()
                .unwrap()
                .unwrap_b()
        }

        let config = Ipv6::DUMMY_CONFIG;
        let mut ctx = DummyEventDispatcherBuilder::default().build::<DummyEventDispatcher>();
        let hw_mtu = 5000;
        let device = ctx.state.add_ethernet_device(local_mac(), hw_mtu);
        let device_id = device.try_into().expect("expected ethernet ID");
        let src_mac = Mac::new([10, 11, 12, 13, 14, 15]);
        let src_ip = src_mac.to_ipv6_link_local().addr();

        crate::device::initialize_device(&mut ctx, device);

        // Receive a new RA with a valid MTU option (but the new MTU should only
        // be 5000 as that is the max MTU of the device).

        let mut icmpv6_packet_buf = packet_buf(src_ip.get(), config.local_ip.get(), 5781);
        let icmpv6_packet = icmpv6_packet_buf
            .parse_with::<_, Icmpv6Packet<_>>(IcmpParseArgs::new(src_ip, config.local_ip))
            .unwrap();
        ctx.receive_ndp_packet(
            device,
            Ipv6SourceAddr::from_witness(src_ip).unwrap(),
            config.local_ip,
            icmpv6_packet.unwrap_ndp(),
        );
        assert_eq!(get_counter_val(&mut ctx, "ndp::rx_router_advertisement"), 1);
        let ndp_state = StateContext::<NdpState<EthernetLinkDevice>, _>::get_state_mut_with(
            &mut ctx, device_id,
        );
        assert!(ndp_state.has_default_router(&src_ip));
        assert_eq!(get_mtu(&ctx, device), hw_mtu);

        // Receive a new RA with an invalid MTU option (value is lower than IPv6
        // min MTU).

        let mut icmpv6_packet_buf =
            packet_buf(src_ip.get(), config.local_ip.get(), u32::from(Ipv6::MINIMUM_LINK_MTU) - 1);
        let icmpv6_packet = icmpv6_packet_buf
            .parse_with::<_, Icmpv6Packet<_>>(IcmpParseArgs::new(src_ip, config.local_ip))
            .unwrap();
        ctx.receive_ndp_packet(
            device,
            Ipv6SourceAddr::from_witness(src_ip).unwrap(),
            config.local_ip,
            icmpv6_packet.unwrap_ndp(),
        );
        assert_eq!(get_counter_val(&mut ctx, "ndp::rx_router_advertisement"), 2);
        let ndp_state = StateContext::<NdpState<EthernetLinkDevice>, _>::get_state_mut_with(
            &mut ctx, device_id,
        );
        assert!(ndp_state.has_default_router(&src_ip));
        assert_eq!(get_mtu(&ctx, device), hw_mtu);

        // Receive a new RA with a valid MTU option (value is exactly IPv6 min
        // MTU).

        let mut icmpv6_packet_buf =
            packet_buf(src_ip.get(), config.local_ip.get(), Ipv6::MINIMUM_LINK_MTU.into());
        let icmpv6_packet = icmpv6_packet_buf
            .parse_with::<_, Icmpv6Packet<_>>(IcmpParseArgs::new(src_ip, config.local_ip))
            .unwrap();
        ctx.receive_ndp_packet(
            device,
            Ipv6SourceAddr::from_witness(src_ip).unwrap(),
            config.local_ip,
            icmpv6_packet.unwrap_ndp(),
        );
        assert_eq!(get_counter_val(&mut ctx, "ndp::rx_router_advertisement"), 3);
        let ndp_state = StateContext::<NdpState<EthernetLinkDevice>, _>::get_state_mut_with(
            &mut ctx, device_id,
        );
        assert!(ndp_state.has_default_router(&src_ip));
        assert_eq!(get_mtu(&ctx, device), Ipv6::MINIMUM_LINK_MTU.into());
    }

    #[test]
    fn test_receiving_router_advertisement_prefix_option() {
        fn packet_buf(
            src_ip: Ipv6Addr,
            dst_ip: Ipv6Addr,
            prefix: Ipv6Addr,
            prefix_length: u8,
            on_link_flag: bool,
            autonomous_address_configuration_flag: bool,
            valid_lifetime: u32,
            preferred_lifetime: u32,
        ) -> Buf<Vec<u8>> {
            let p = PrefixInformation::new(
                prefix_length,
                on_link_flag,
                autonomous_address_configuration_flag,
                valid_lifetime,
                preferred_lifetime,
                prefix,
            );
            let options = &[NdpOptionBuilder::PrefixInformation(p)];
            OptionSequenceBuilder::new(options.iter())
                .into_serializer()
                .encapsulate(IcmpPacketBuilder::<Ipv6, &[u8], _>::new(
                    src_ip,
                    dst_ip,
                    IcmpUnusedCode,
                    RouterAdvertisement::new(1, false, false, 0, 4, 5),
                ))
                .serialize_vec_outer()
                .unwrap()
                .unwrap_b()
        }

        let config = Ipv6::DUMMY_CONFIG;
        let mut ctx = DummyEventDispatcherBuilder::from_config(config.clone())
            .build::<DummyEventDispatcher>();
        let device = DeviceId::new_ethernet(0);
        let device_id = device.try_into().expect("expected ethernet ID");
        let src_mac = Mac::new([10, 11, 12, 13, 14, 15]);
        let src_ip = src_mac.to_ipv6_link_local().addr().get();
        let prefix = Ipv6Addr::from([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 192, 168, 0, 0]);
        let prefix_length = 120;
        let subnet = Subnet::new(prefix, prefix_length).unwrap();

        // Receive a new RA with new prefix.

        let mut icmpv6_packet_buf =
            packet_buf(src_ip, config.local_ip.get(), prefix, prefix_length, true, false, 100, 0);
        let icmpv6_packet = icmpv6_packet_buf
            .parse_with::<_, Icmpv6Packet<_>>(IcmpParseArgs::new(src_ip, config.local_ip))
            .unwrap();
        ctx.receive_ndp_packet(
            device,
            src_ip.try_into().unwrap(),
            config.local_ip,
            icmpv6_packet.unwrap_ndp(),
        );
        assert_eq!(get_counter_val(&mut ctx, "ndp::rx_router_advertisement"), 1);
        let ndp_state = StateContext::<NdpState<EthernetLinkDevice>, _>::get_state_mut_with(
            &mut ctx, device_id,
        );
        // Prefix should be in our list now.
        assert!(ndp_state.has_prefix(&subnet));
        // Invalidation timeout should be set.
        assert_eq!(ctx.dispatcher.timer_events().count(), 1);

        // Receive a RA with same prefix but valid_lifetime = 0;

        let mut icmpv6_packet_buf =
            packet_buf(src_ip, config.local_ip.get(), prefix, prefix_length, true, false, 0, 0);
        let icmpv6_packet = icmpv6_packet_buf
            .parse_with::<_, Icmpv6Packet<_>>(IcmpParseArgs::new(src_ip, config.local_ip))
            .unwrap();
        ctx.receive_ndp_packet(
            device,
            src_ip.try_into().unwrap(),
            config.local_ip,
            icmpv6_packet.unwrap_ndp(),
        );
        assert_eq!(get_counter_val(&mut ctx, "ndp::rx_router_advertisement"), 2);
        let ndp_state = StateContext::<NdpState<EthernetLinkDevice>, _>::get_state_mut_with(
            &mut ctx, device_id,
        );
        // Should remove the prefix from our list now.
        assert!(!ndp_state.has_prefix(&subnet));
        // Invalidation timeout should be unset.
        assert_empty(ctx.dispatcher.timer_events());

        // Receive a new RA with new prefix (same as before but new since it
        // isn't in our list right now).

        let mut icmpv6_packet_buf =
            packet_buf(src_ip, config.local_ip.get(), prefix, prefix_length, true, false, 100, 0);
        let icmpv6_packet = icmpv6_packet_buf
            .parse_with::<_, Icmpv6Packet<_>>(IcmpParseArgs::new(src_ip, config.local_ip))
            .unwrap();
        ctx.receive_ndp_packet(
            device,
            src_ip.try_into().unwrap(),
            config.local_ip,
            icmpv6_packet.unwrap_ndp(),
        );
        assert_eq!(get_counter_val(&mut ctx, "ndp::rx_router_advertisement"), 3);
        let ndp_state = StateContext::<NdpState<EthernetLinkDevice>, _>::get_state_mut_with(
            &mut ctx, device_id,
        );
        // Prefix should be in our list now.
        assert!(ndp_state.has_prefix(&subnet));
        // Invalidation timeout should be set.
        assert_eq!(ctx.dispatcher.timer_events().count(), 1);

        // Receive the exact same RA as before.

        let mut icmpv6_packet_buf =
            packet_buf(src_ip, config.local_ip.get(), prefix, prefix_length, true, false, 100, 0);
        let icmpv6_packet = icmpv6_packet_buf
            .parse_with::<_, Icmpv6Packet<_>>(IcmpParseArgs::new(src_ip, config.local_ip))
            .unwrap();
        ctx.receive_ndp_packet(
            device,
            src_ip.try_into().unwrap(),
            config.local_ip,
            icmpv6_packet.unwrap_ndp(),
        );
        assert_eq!(get_counter_val(&mut ctx, "ndp::rx_router_advertisement"), 4);
        let ndp_state = StateContext::<NdpState<EthernetLinkDevice>, _>::get_state_mut_with(
            &mut ctx, device_id,
        );
        // Prefix should be in our list still.
        assert!(ndp_state.has_prefix(&subnet));
        // Invalidation timeout should still be set.
        assert_eq!(ctx.dispatcher.timer_events().count(), 1);

        // Timeout the prefix.

        assert_eq!(
            trigger_next_timer(&mut ctx).unwrap(),
            NdpTimerId::new_prefix_invalidation(device_id.into(), subnet).into()
        );

        // Prefix should no longer be in our list.
        let ndp_state = StateContext::<NdpState<EthernetLinkDevice>, _>::get_state_mut_with(
            &mut ctx, device_id,
        );
        assert!(!ndp_state.has_prefix(&subnet));

        // No more timers.
        assert_eq!(trigger_next_timer(&mut ctx), None);
    }

    #[test]
    fn test_host_send_router_solicitations() {
        fn validate_params(
            src_mac: Mac,
            src_ip: Ipv6Addr,
            message: RouterSolicitation,
            code: IcmpUnusedCode,
        ) {
            let dummy_config = Ipv6::DUMMY_CONFIG;
            assert_eq!(src_mac, dummy_config.local_mac.get());
            assert_eq!(src_ip, dummy_config.local_mac.to_ipv6_link_local().addr().get());
            assert_eq!(message, RouterSolicitation::default());
            assert_eq!(code, IcmpUnusedCode);
        }

        // By default, we should send `MAX_RTR_SOLICITATIONS` number of Router
        // Solicitation messages.

        let dummy_config = Ipv6::DUMMY_CONFIG;

        let mut stack_builder = StackStateBuilder::default();
        let mut ipv6_config = crate::device::Ipv6DeviceConfiguration::default();
        ipv6_config.set_dad_transmits(None);
        stack_builder.device_builder().set_default_ipv6_config(ipv6_config);
        let mut ctx = Ctx::new(stack_builder.build(), DummyEventDispatcher::default());

        assert_empty(ctx.dispatcher.frames_sent());
        let device_id =
            ctx.state.add_ethernet_device(dummy_config.local_mac, Ipv6::MINIMUM_LINK_MTU.into());
        crate::device::initialize_device(&mut ctx, device_id);
        assert_empty(ctx.dispatcher.frames_sent());

        let time = ctx.now();
        assert_eq!(
            trigger_next_timer(&mut ctx).unwrap(),
            NdpTimerId::new_router_solicitation(
                device_id.try_into().expect("expected ethernet ID")
            )
            .into()
        );
        // Initial router solicitation should be a random delay between 0 and
        // `MAX_RTR_SOLICITATION_DELAY`.
        assert!(ctx.now().duration_since(time) < MAX_RTR_SOLICITATION_DELAY);
        assert_eq!(ctx.dispatcher.frames_sent().len(), 1);
        let (src_mac, _, src_ip, _, _, message, code) =
            parse_icmp_packet_in_ip_packet_in_ethernet_frame::<Ipv6, _, RouterSolicitation, _>(
                &ctx.dispatcher.frames_sent()[0].1,
                |_| {},
            )
            .unwrap();
        validate_params(src_mac, src_ip, message, code);

        // Should get 2 more router solicitation messages
        let time = ctx.now();
        assert_eq!(
            trigger_next_timer(&mut ctx).unwrap(),
            NdpTimerId::new_router_solicitation(
                device_id.try_into().expect("expected ethernet ID")
            )
            .into()
        );
        assert_eq!(ctx.now().duration_since(time), RTR_SOLICITATION_INTERVAL);
        let (src_mac, _, src_ip, _, _, message, code) =
            parse_icmp_packet_in_ip_packet_in_ethernet_frame::<Ipv6, _, RouterSolicitation, _>(
                &ctx.dispatcher.frames_sent()[1].1,
                |_| {},
            )
            .unwrap();
        validate_params(src_mac, src_ip, message, code);

        // Before the next one, lets assign an IP address (DAD won't be
        // performed so it will be assigned immediately. The router solicitation
        // message will now use the new assigned IP as the source.
        add_ip_addr_subnet(
            &mut ctx,
            device_id,
            AddrSubnet::new(dummy_config.local_ip.get(), 128).unwrap(),
        )
        .unwrap();
        let time = ctx.now();
        assert_eq!(
            trigger_next_timer(&mut ctx).unwrap(),
            NdpTimerId::new_router_solicitation(
                device_id.try_into().expect("expected ethernet ID")
            )
            .into()
        );
        assert_eq!(ctx.now().duration_since(time), RTR_SOLICITATION_INTERVAL);
        let (src_mac, _, src_ip, _, _, message, code) =
            parse_icmp_packet_in_ip_packet_in_ethernet_frame::<Ipv6, _, RouterSolicitation, _>(
                &ctx.dispatcher.frames_sent()[2].1,
                |p| {
                    // We should have a source link layer option now because we
                    // have a source IP address set.
                    assert_eq!(p.body().iter().count(), 1);
                    if let Some(ll) = get_source_link_layer_option::<Mac, _>(p.body()) {
                        assert_eq!(ll, dummy_config.local_mac.get());
                    } else {
                        panic!("Should have a source link layer option");
                    }
                },
            )
            .unwrap();
        assert_eq!(src_mac, dummy_config.local_mac.get());
        assert_eq!(src_ip, dummy_config.local_ip.get());
        assert_eq!(message, RouterSolicitation::default());
        assert_eq!(code, IcmpUnusedCode);

        // No more timers.
        assert_eq!(trigger_next_timer(&mut ctx), None);
        // Should have only sent 3 packets (Router solicitations).
        assert_eq!(ctx.dispatcher.frames_sent().len(), 3);

        // Configure MAX_RTR_SOLICITATIONS in the stack.

        let mut stack_builder = StackStateBuilder::default();
        let mut ipv6_config = crate::device::Ipv6DeviceConfiguration::default();
        ipv6_config.set_dad_transmits(None);
        stack_builder.device_builder().set_default_ipv6_config(ipv6_config);
        let mut ndp_config = crate::device::ndp::NdpConfiguration::default();
        ndp_config.set_max_router_solicitations(NonZeroU8::new(2));
        stack_builder.device_builder().set_default_ndp_config(ndp_config);
        let mut ctx = Ctx::new(stack_builder.build(), DummyEventDispatcher::default());

        assert_empty(ctx.dispatcher.frames_sent());
        let device_id =
            ctx.state.add_ethernet_device(dummy_config.local_mac, Ipv6::MINIMUM_LINK_MTU.into());
        crate::device::initialize_device(&mut ctx, device_id);
        assert_empty(ctx.dispatcher.frames_sent());

        let time = ctx.now();
        assert_eq!(
            trigger_next_timer(&mut ctx).unwrap(),
            NdpTimerId::new_router_solicitation(
                device_id.try_into().expect("expected ethernet ID")
            )
            .into()
        );
        // Initial router solicitation should be a random delay between 0 and
        // `MAX_RTR_SOLICITATION_DELAY`.
        assert!(ctx.now().duration_since(time) < MAX_RTR_SOLICITATION_DELAY);
        assert_eq!(ctx.dispatcher.frames_sent().len(), 1);

        // Should trigger 1 more router solicitations
        let time = ctx.now();
        assert_eq!(
            trigger_next_timer(&mut ctx).unwrap(),
            NdpTimerId::new_router_solicitation(
                device_id.try_into().expect("expected ethernet ID")
            )
            .into()
        );
        assert_eq!(ctx.now().duration_since(time), RTR_SOLICITATION_INTERVAL);
        assert_eq!(ctx.dispatcher.frames_sent().len(), 2);

        // Each packet would be the same.
        for f in ctx.dispatcher.frames_sent() {
            let (src_mac, _, src_ip, _, _, message, code) =
                parse_icmp_packet_in_ip_packet_in_ethernet_frame::<Ipv6, _, RouterSolicitation, _>(
                    &f.1,
                    |_| {},
                )
                .unwrap();
            validate_params(src_mac, src_ip, message, code);
        }

        // No more timers.
        assert_eq!(trigger_next_timer(&mut ctx), None);
    }

    #[test]
    fn test_router_solicitation_on_routing_enabled_changes() {
        // Make sure that when an interface goes from host -> router, it stops
        // sending Router Solicitations, and starts sending them when it goes
        // form router -> host as routers should not send Router Solicitation
        // messages, but hosts should.

        let dummy_config = Ipv6::DUMMY_CONFIG;

        // If netstack is not set to forward packets, make sure router
        // solicitations do not get cancelled when we enable forwarding on the
        // device.

        let mut state_builder = StackStateBuilder::default();
        let _: &mut Ipv6StateBuilder = state_builder.ipv6_builder().forward(false);
        let mut ipv6_config = crate::device::Ipv6DeviceConfiguration::default();
        ipv6_config.set_dad_transmits(None);
        state_builder.device_builder().set_default_ipv6_config(ipv6_config);
        let mut ctx = DummyEventDispatcherBuilder::default()
            .build_with(state_builder, DummyEventDispatcher::default());

        assert_empty(ctx.dispatcher.frames_sent());
        assert_empty(ctx.dispatcher.timer_events());

        let device =
            ctx.state.add_ethernet_device(dummy_config.local_mac, Ipv6::MINIMUM_LINK_MTU.into());
        crate::device::initialize_device(&mut ctx, device);
        let timer_id =
            NdpTimerId::new_router_solicitation(device.try_into().expect("expected ethernet ID"))
                .into();

        // Send the first router solicitation.
        assert_empty(ctx.dispatcher.frames_sent());
        let timers: Vec<(&DummyInstant, &TimerId)> =
            ctx.dispatcher.timer_events().filter(|x| *x.1 == timer_id).collect();
        assert_eq!(timers.len(), 1);

        assert_eq!(trigger_next_timer(&mut ctx).unwrap(), timer_id);

        // Should have sent a router solicitation and still have the timer
        // setup.
        assert_eq!(ctx.dispatcher.frames_sent().len(), 1);
        let (_, _dst_mac, _, _, _, _, _) =
            parse_icmp_packet_in_ip_packet_in_ethernet_frame::<Ipv6, _, RouterSolicitation, _>(
                &ctx.dispatcher.frames_sent()[0].1,
                |_| {},
            )
            .unwrap();
        let timers: Vec<(&DummyInstant, &TimerId)> =
            ctx.dispatcher.timer_events().filter(|x| *x.1 == timer_id).collect();
        assert_eq!(timers.len(), 1);
        // Capture the instant when the timer was supposed to fire so we can
        // make sure that a new timer doesn't replace the current one.
        let instant = timers[0].0.clone();

        // Enable routing on device.
        set_routing_enabled::<_, Ipv6>(&mut ctx, device, true)
            .expect("error setting routing enabled");
        assert!(is_routing_enabled::<_, Ipv6>(&ctx, device));

        // Should have not send any new packets and still have the original
        // router solicitation timer set.
        assert_eq!(ctx.dispatcher.frames_sent().len(), 1);
        let timers: Vec<(&DummyInstant, &TimerId)> =
            ctx.dispatcher.timer_events().filter(|x| *x.1 == timer_id).collect();
        assert_eq!(timers.len(), 1);
        assert_eq!(*timers[0].0, instant);

        // Now make the netstack and a device actually routing capable.

        let mut state_builder = StackStateBuilder::default();
        let _: &mut Ipv6StateBuilder = state_builder.ipv6_builder().forward(true);
        let mut ipv6_config = crate::device::Ipv6DeviceConfiguration::default();
        ipv6_config.set_dad_transmits(None);
        state_builder.device_builder().set_default_ipv6_config(ipv6_config);
        let mut ctx = DummyEventDispatcherBuilder::default()
            .build_with(state_builder, DummyEventDispatcher::default());

        assert_empty(ctx.dispatcher.frames_sent());
        assert_empty(ctx.dispatcher.timer_events());

        let device =
            ctx.state.add_ethernet_device(dummy_config.local_mac, Ipv6::MINIMUM_LINK_MTU.into());
        crate::device::initialize_device(&mut ctx, device);
        let timer_id =
            NdpTimerId::new_router_solicitation(device.try_into().expect("expected ethernet ID"))
                .into();

        // Send the first router solicitation.
        assert_empty(ctx.dispatcher.frames_sent());
        let timers: Vec<(&DummyInstant, &TimerId)> =
            ctx.dispatcher.timer_events().filter(|x| *x.1 == timer_id).collect();
        assert_eq!(timers.len(), 1);

        assert_eq!(trigger_next_timer(&mut ctx).unwrap(), timer_id);

        // Should have sent a frame and have a router solicitation timer setup.
        assert_eq!(ctx.dispatcher.frames_sent().len(), 1);
        assert_matches::assert_matches!(
            parse_icmp_packet_in_ip_packet_in_ethernet_frame::<Ipv6, _, RouterSolicitation, _>(
                &ctx.dispatcher.frames_sent()[0].1,
                |_| {},
            ),
            Ok((_, _, _, _, _, _, _))
        );
        assert_eq!(ctx.dispatcher.timer_events().filter(|x| *x.1 == timer_id).count(), 1);

        // Enable routing on the device.
        set_routing_enabled::<_, Ipv6>(&mut ctx, device, true)
            .expect("error setting routing enabled");
        assert!(is_routing_enabled::<_, Ipv6>(&ctx, device));

        // Should have not sent any new packets, but unset the router
        // solicitation timer.
        assert_eq!(ctx.dispatcher.frames_sent().len(), 1);
        assert_empty(ctx.dispatcher.timer_events().filter(|x| *x.1 == timer_id));

        // Unsetting routing should succeed.
        set_routing_enabled::<_, Ipv6>(&mut ctx, device, false)
            .expect("error setting routing enabled");
        assert!(!is_routing_enabled::<_, Ipv6>(&ctx, device));
        assert_eq!(ctx.dispatcher.frames_sent().len(), 1);
        let timers: Vec<(&DummyInstant, &TimerId)> =
            ctx.dispatcher.timer_events().filter(|x| *x.1 == timer_id).collect();
        assert_eq!(timers.len(), 1);

        // Send the first router solicitation after being turned into a host.
        assert_eq!(trigger_next_timer(&mut ctx).unwrap(), timer_id);

        // Should have sent a router solicitation.
        assert_eq!(ctx.dispatcher.frames_sent().len(), 2);
        assert_matches::assert_matches!(
            parse_icmp_packet_in_ip_packet_in_ethernet_frame::<Ipv6, _, RouterSolicitation, _>(
                &ctx.dispatcher.frames_sent()[1].1,
                |_| {},
            ),
            Ok((_, _, _, _, _, _, _))
        );
        assert_eq!(ctx.dispatcher.timer_events().filter(|x| *x.1 == timer_id).count(), 1);
    }

    #[test]
    fn test_set_ndp_config_dup_addr_detect_transmits() {
        // Test that updating the DupAddrDetectTransmits parameter on an
        // interface updates the number of DAD messages (NDP Neighbor
        // Solicitations) sent before concluding that an address is not a
        // duplicate.

        let dummy_config = Ipv6::DUMMY_CONFIG;
        let mut ctx = DummyEventDispatcherBuilder::default().build::<DummyEventDispatcher>();
        let device =
            ctx.state.add_ethernet_device(dummy_config.local_mac, Ipv6::MINIMUM_LINK_MTU.into());
        crate::device::initialize_device(&mut ctx, device);
        assert_empty(ctx.dispatcher.frames_sent());
        assert_empty(ctx.dispatcher.timer_events());

        // Updating the IP should resolve immediately since DAD is turned off by
        // `DummyEventDispatcherBuilder::build`.
        add_ip_addr_subnet(
            &mut ctx,
            device,
            AddrSubnet::new(dummy_config.local_ip.get(), 128).unwrap(),
        )
        .unwrap();
        assert_eq!(
            NdpContext::<EthernetLinkDevice>::get_ip_device_state(
                &ctx,
                device.try_into().expect("expected ethernet ID")
            )
            .find_addr(&dummy_config.local_ip.try_into().unwrap())
            .unwrap()
            .state,
            AddressState::Assigned
        );
        assert_empty(ctx.dispatcher.frames_sent());
        assert_empty(ctx.dispatcher.timer_events());

        // Enable DAD for the device.
        const DUP_ADDR_DETECT_TRANSMITS: u8 = 3;
        let mut ipv6_config = crate::device::Ipv6DeviceConfiguration::default();
        ipv6_config.set_dad_transmits(NonZeroU8::new(DUP_ADDR_DETECT_TRANSMITS));
        crate::device::set_ipv6_configuration(&mut ctx, device, ipv6_config.clone());

        // Updating the IP should start the DAD process.
        add_ip_addr_subnet(
            &mut ctx,
            device,
            AddrSubnet::new(dummy_config.remote_ip.get(), 128).unwrap(),
        )
        .unwrap();
        assert_eq!(
            NdpContext::<EthernetLinkDevice>::get_ip_device_state(
                &ctx,
                device.try_into().expect("expected ethernet ID")
            )
            .find_addr(&dummy_config.local_ip.try_into().unwrap())
            .unwrap()
            .state,
            AddressState::Assigned
        );
        assert_eq!(
            NdpContext::<EthernetLinkDevice>::get_ip_device_state(
                &ctx,
                device.try_into().expect("expected ethernet ID")
            )
            .find_addr(&dummy_config.remote_ip.try_into().unwrap())
            .unwrap()
            .state,
            AddressState::Tentative {
                dad_transmits_remaining: NonZeroU8::new(DUP_ADDR_DETECT_TRANSMITS - 1)
            }
        );
        assert_eq!(ctx.dispatcher.frames_sent().len(), 1);
        assert_eq!(ctx.dispatcher.timer_events().count(), 1);

        // Disable DAD during DAD.
        ipv6_config.set_dad_transmits(None);
        crate::device::set_ipv6_configuration(&mut ctx, device, ipv6_config.clone());
        let expected_timer_id = dad_timer_id(
            device.try_into().expect("expected ethernet ID"),
            dummy_config.remote_ip.try_into().unwrap(),
        );
        // Allow already started DAD to complete (2 more more NS, 3 more timers).
        assert_eq!(trigger_next_timer(&mut ctx).unwrap(), expected_timer_id);
        assert_eq!(ctx.dispatcher.frames_sent().len(), 2);
        assert_eq!(trigger_next_timer(&mut ctx).unwrap(), expected_timer_id);
        assert_eq!(ctx.dispatcher.frames_sent().len(), 3);
        assert_eq!(trigger_next_timer(&mut ctx).unwrap(), expected_timer_id);
        assert_eq!(ctx.dispatcher.frames_sent().len(), 3);
        assert_eq!(
            NdpContext::<EthernetLinkDevice>::get_ip_device_state(
                &ctx,
                device.try_into().expect("expected ethernet ID")
            )
            .find_addr(&dummy_config.remote_ip.try_into().unwrap())
            .unwrap()
            .state,
            AddressState::Assigned
        );

        // Updating the IP should resolve immediately since DAD has just been
        // turned off.
        let new_ip = Ipv6::get_other_ip_address(3);
        add_ip_addr_subnet(&mut ctx, device, AddrSubnet::new(new_ip.get(), 128).unwrap()).unwrap();
        assert_eq!(
            NdpContext::<EthernetLinkDevice>::get_ip_device_state(
                &ctx,
                device.try_into().expect("expected ethernet ID")
            )
            .find_addr(&dummy_config.local_ip.try_into().unwrap())
            .unwrap()
            .state,
            AddressState::Assigned
        );
        assert_eq!(
            NdpContext::<EthernetLinkDevice>::get_ip_device_state(
                &ctx,
                device.try_into().expect("expected ethernet ID")
            )
            .find_addr(&dummy_config.remote_ip.try_into().unwrap())
            .unwrap()
            .state,
            AddressState::Assigned
        );
        assert_eq!(
            NdpContext::<EthernetLinkDevice>::get_ip_device_state(
                &ctx,
                device.try_into().expect("expected ethernet ID")
            )
            .find_addr(&new_ip.try_into().unwrap())
            .unwrap()
            .state,
            AddressState::Assigned
        );
    }

    #[test]
    fn test_receiving_neighbor_advertisements() {
        fn test_receiving_na_from_known_neighbor(
            ctx: &mut Ctx<DummyEventDispatcher>,
            src_ip: Ipv6Addr,
            dst_ip: SpecifiedAddr<Ipv6Addr>,
            device: DeviceId,
            router_flag: bool,
            solicited_flag: bool,
            override_flag: bool,
            mac: Option<Mac>,
            expected_state: NeighborEntryState,
            expected_router: bool,
            expected_link_addr: Option<Mac>,
        ) {
            let mut buf = neighbor_advertisement_message(
                src_ip,
                dst_ip.get(),
                router_flag,
                solicited_flag,
                override_flag,
                mac,
            );
            let packet =
                buf.parse_with::<_, Icmpv6Packet<_>>(IcmpParseArgs::new(src_ip, dst_ip)).unwrap();
            ctx.receive_ndp_packet(device, src_ip.try_into().unwrap(), dst_ip, packet.unwrap_ndp());

            let neighbor_state =
                StateContext::<NdpState<EthernetLinkDevice>, _>::get_state_mut_with(
                    ctx,
                    device.try_into().expect("expected ethernet ID"),
                )
                .neighbors
                .get_neighbor_state(&src_ip.try_into().unwrap())
                .unwrap();
            assert_eq!(neighbor_state.state, expected_state);
            assert_eq!(neighbor_state.is_router, expected_router);
            assert_eq!(neighbor_state.link_address, expected_link_addr);
        }

        let config = Ipv6::DUMMY_CONFIG;
        let mut ctx = DummyEventDispatcherBuilder::default().build::<DummyEventDispatcher>();
        let device = ctx.state.add_ethernet_device(config.local_mac, Ipv6::MINIMUM_LINK_MTU.into());
        crate::device::initialize_device(&mut ctx, device);

        let neighbor_mac = config.remote_mac.get();
        let neighbor_ip = neighbor_mac.to_ipv6_link_local().addr();
        let all_nodes_addr = Ipv6::ALL_NODES_LINK_LOCAL_MULTICAST_ADDRESS.into_specified();

        // Should not know about the neighbor yet.
        assert_eq!(
            StateContext::<NdpState<EthernetLinkDevice>, _>::get_state_mut_with(
                &mut ctx,
                device.try_into().expect("expected ethernet ID")
            )
            .neighbors
            .get_neighbor_state(&neighbor_ip.get()),
            None
        );

        // Receiving unsolicited NA from a neighbor we don't care about yet
        // should do nothing.

        // Receive the NA.
        let mut buf = neighbor_advertisement_message(
            neighbor_ip.get(),
            all_nodes_addr.get(),
            false,
            false,
            false,
            None,
        );
        let packet = buf
            .parse_with::<_, Icmpv6Packet<_>>(IcmpParseArgs::new(neighbor_ip, all_nodes_addr))
            .unwrap();
        ctx.receive_ndp_packet(
            device,
            Ipv6SourceAddr::from_witness(neighbor_ip).unwrap(),
            all_nodes_addr,
            packet.unwrap_ndp(),
        );

        // We still do not know about the neighbor since the NA was unsolicited
        // and we never were interested in the neighbor yet.
        assert_eq!(
            StateContext::<NdpState<EthernetLinkDevice>, _>::get_state_mut_with(
                &mut ctx,
                device.try_into().expect("expected ethernet ID")
            )
            .neighbors
            .get_neighbor_state(&neighbor_ip),
            None
        );

        // Receiving solicited NA from a neighbor we don't care about yet should
        // do nothing (should never happen).

        // Receive the NA.
        let mut buf = neighbor_advertisement_message(
            neighbor_ip.get(),
            all_nodes_addr.get(),
            false,
            true,
            false,
            None,
        );
        let packet = buf
            .parse_with::<_, Icmpv6Packet<_>>(IcmpParseArgs::new(neighbor_ip, all_nodes_addr))
            .unwrap();
        ctx.receive_ndp_packet(
            device,
            Ipv6SourceAddr::from_witness(neighbor_ip).unwrap(),
            all_nodes_addr,
            packet.unwrap_ndp(),
        );

        // We still do not know about the neighbor since the NA was unsolicited
        // and we never were interested in the neighbor yet.
        assert_eq!(
            StateContext::<NdpState<EthernetLinkDevice>, _>::get_state_mut_with(
                &mut ctx,
                device.try_into().expect("expected ethernet ID")
            )
            .neighbors
            .get_neighbor_state(&neighbor_ip),
            None
        );

        // Receiving solicited NA from a neighbor we are trying to resolve, but
        // no target link addr.
        //
        // Should do nothing (still INCOMPLETE).

        // Create incomplete neighbor entry.
        let neighbors = &mut StateContext::<NdpState<EthernetLinkDevice>, _>::get_state_mut_with(
            &mut ctx,
            device.try_into().expect("expected ethernet ID"),
        )
        .neighbors;
        neighbors.add_incomplete_neighbor_state(neighbor_ip.get());

        test_receiving_na_from_known_neighbor(
            &mut ctx,
            neighbor_ip.get(),
            config.local_ip,
            device,
            false,
            true,
            false,
            None,
            NeighborEntryState::Incomplete { transmit_counter: 1 },
            false,
            None,
        );

        // Receiving solicited NA from a neighbor we are resolving, but with
        // target link addr.
        //
        // Should update link layer address and set state to REACHABLE.

        test_receiving_na_from_known_neighbor(
            &mut ctx,
            neighbor_ip.get(),
            config.local_ip,
            device,
            false,
            true,
            false,
            Some(neighbor_mac),
            NeighborEntryState::Reachable,
            false,
            Some(neighbor_mac),
        );

        // Receive unsolicited NA from a neighbor with router flag updated (no
        // target link addr).
        //
        // Should update is_router to true.

        test_receiving_na_from_known_neighbor(
            &mut ctx,
            neighbor_ip.get(),
            config.local_ip,
            device,
            true,
            false,
            false,
            None,
            NeighborEntryState::Reachable,
            true,
            Some(neighbor_mac),
        );

        // Receive unsolicited NA from a neighbor without router flag set and
        // same target link addr.
        //
        // Should update is_router, state should be unchanged.

        test_receiving_na_from_known_neighbor(
            &mut ctx,
            neighbor_ip.get(),
            config.local_ip,
            device,
            false,
            false,
            false,
            Some(neighbor_mac),
            NeighborEntryState::Reachable,
            false,
            Some(neighbor_mac),
        );

        // Receive unsolicited NA from a neighbor with new target link addr.
        //
        // Should NOT update link layer addr, but set state to STALE.

        let new_mac = Mac::new([99, 98, 97, 96, 95, 94]);

        test_receiving_na_from_known_neighbor(
            &mut ctx,
            neighbor_ip.get(),
            config.local_ip,
            device,
            false,
            false,
            false,
            Some(new_mac),
            NeighborEntryState::Stale,
            false,
            Some(neighbor_mac),
        );

        // Receive unsolicited NA from a neighbor with new target link addr and
        // override set.
        //
        // Should update link layer addr and set state to STALE.

        test_receiving_na_from_known_neighbor(
            &mut ctx,
            neighbor_ip.get(),
            config.local_ip,
            device,
            false,
            false,
            true,
            Some(new_mac),
            NeighborEntryState::Stale,
            false,
            Some(new_mac),
        );

        // Receive solicited NA from a neighbor with the same link layer addr.
        //
        // Should not update link layer addr, but set state to REACHABLE.

        test_receiving_na_from_known_neighbor(
            &mut ctx,
            neighbor_ip.get(),
            config.local_ip,
            device,
            false,
            true,
            false,
            Some(new_mac),
            NeighborEntryState::Reachable,
            false,
            Some(new_mac),
        );

        // Receive unsolicited NA from a neighbor with new target link addr and
        // override set.
        //
        // Should update link layer addr, and set state to Stale.

        test_receiving_na_from_known_neighbor(
            &mut ctx,
            neighbor_ip.get(),
            config.local_ip,
            device,
            false,
            false,
            true,
            Some(neighbor_mac),
            NeighborEntryState::Stale,
            false,
            Some(neighbor_mac),
        );

        // Receive solicited NA from a neighbor with new target link addr and
        // override set.
        //
        // Should set state to Reachable.

        test_receiving_na_from_known_neighbor(
            &mut ctx,
            neighbor_ip.get(),
            config.local_ip,
            device,
            false,
            true,
            true,
            Some(neighbor_mac),
            NeighborEntryState::Reachable,
            false,
            Some(neighbor_mac),
        );

        // Receive unsolicited NA from a neighbor with no target link addr and
        // override set.
        //
        // Should do nothing.

        test_receiving_na_from_known_neighbor(
            &mut ctx,
            neighbor_ip.get(),
            config.local_ip,
            device,
            false,
            false,
            true,
            None,
            NeighborEntryState::Reachable,
            false,
            Some(neighbor_mac),
        );
    }

    fn slaac_packet_buf(
        src_ip: Ipv6Addr,
        dst_ip: Ipv6Addr,
        prefix: Ipv6Addr,
        prefix_length: u8,
        on_link_flag: bool,
        autonomous_address_configuration_flag: bool,
        valid_lifetime: u32,
        preferred_lifetime: u32,
    ) -> Buf<Vec<u8>> {
        let p = PrefixInformation::new(
            prefix_length,
            on_link_flag,
            autonomous_address_configuration_flag,
            valid_lifetime,
            preferred_lifetime,
            prefix,
        );
        let options = &[NdpOptionBuilder::PrefixInformation(p)];
        OptionSequenceBuilder::new(options.iter())
            .into_serializer()
            .encapsulate(IcmpPacketBuilder::<Ipv6, &[u8], _>::new(
                src_ip,
                dst_ip,
                IcmpUnusedCode,
                RouterAdvertisement::new(0, false, false, 0, 0, 0),
            ))
            .serialize_vec_outer()
            .unwrap()
            .unwrap_b()
    }

    #[test]
    fn test_router_stateless_address_autoconfiguration() {
        // Routers should not perform SLAAC for global addresses.

        let config = Ipv6::DUMMY_CONFIG;
        let mut state_builder = StackStateBuilder::default();
        let mut ipv6_config = crate::device::Ipv6DeviceConfiguration::default();
        ipv6_config.set_dad_transmits(None);
        state_builder.device_builder().set_default_ipv6_config(ipv6_config);
        let mut ndp_config = NdpConfiguration::default();
        ndp_config.set_max_router_solicitations(None);
        state_builder.device_builder().set_default_ndp_config(ndp_config);
        let _: &mut Ipv6StateBuilder = state_builder.ipv6_builder().forward(true);
        let mut ctx = DummyEventDispatcherBuilder::default()
            .build_with(state_builder, DummyEventDispatcher::default());
        let device = ctx.state.add_ethernet_device(config.local_mac, Ipv6::MINIMUM_LINK_MTU.into());
        crate::device::initialize_device(&mut ctx, device);
        crate::device::set_routing_enabled::<_, Ipv6>(&mut ctx, device, true)
            .expect("error setting routing enabled");

        let src_mac = config.remote_mac;
        let src_ip = src_mac.to_ipv6_link_local().addr().get();
        let prefix = Ipv6Addr::from([1, 2, 3, 4, 5, 6, 7, 8, 0, 0, 0, 0, 0, 0, 0, 0]);
        let prefix_length = 64;
        let mut expected_addr = [1, 2, 3, 4, 5, 6, 7, 8, 0, 0, 0, 0, 0, 0, 0, 0];
        expected_addr[8..].copy_from_slice(&src_mac.to_eui64()[..]);

        // Receive a new RA with new prefix (autonomous).
        //
        // Should not get a new IP.

        let mut icmpv6_packet_buf = slaac_packet_buf(
            src_ip,
            config.local_ip.get(),
            prefix,
            prefix_length,
            true,
            false,
            100,
            0,
        );
        let icmpv6_packet = icmpv6_packet_buf
            .parse_with::<_, Icmpv6Packet<_>>(IcmpParseArgs::new(src_ip, config.local_ip))
            .unwrap();
        ctx.receive_ndp_packet(
            device,
            src_ip.try_into().unwrap(),
            config.local_ip,
            icmpv6_packet.unwrap_ndp(),
        );

        assert_empty(
            NdpContext::<EthernetLinkDevice>::get_ip_device_state(
                &ctx,
                device.try_into().expect("expected ethernet ID"),
            )
            .iter_global_ipv6_addrs(),
        );

        // No timers.
        assert_eq!(trigger_next_timer(&mut ctx), None);
    }

    #[test]
    fn test_host_stateless_address_autoconfiguration() {
        let config = Ipv6::DUMMY_CONFIG;
        let mut ctx = DummyEventDispatcherBuilder::default().build::<DummyEventDispatcher>();
        let device = ctx.state.add_ethernet_device(config.local_mac, Ipv6::MINIMUM_LINK_MTU.into());
        crate::device::initialize_device(&mut ctx, device);

        let src_mac = config.remote_mac;
        let src_ip = src_mac.to_ipv6_link_local().addr().get();
        let prefix = Ipv6Addr::from([1, 2, 3, 4, 5, 6, 7, 8, 0, 0, 0, 0, 0, 0, 0, 0]);
        let prefix_length = 64;
        let subnet = Subnet::new(prefix, prefix_length).unwrap();
        let mut expected_addr = [1, 2, 3, 4, 5, 6, 7, 8, 0, 0, 0, 0, 0, 0, 0, 0];
        expected_addr[8..].copy_from_slice(&config.local_mac.to_eui64()[..]);
        let expected_addr = UnicastAddr::new(Ipv6Addr::from(expected_addr)).unwrap();
        let expected_subnet = Subnet::from_host(*expected_addr, prefix_length).unwrap();

        // Enable DAD for future IPs.
        crate::device::set_ipv6_configuration(
            &mut ctx,
            device,
            crate::device::Ipv6DeviceConfiguration::default(),
        );

        // Receive a new RA with new prefix (not autonomous)

        let mut icmpv6_packet_buf = slaac_packet_buf(
            src_ip,
            config.local_ip.get(),
            prefix,
            prefix_length,
            true,
            false,
            100,
            0,
        );
        let icmpv6_packet = icmpv6_packet_buf
            .parse_with::<_, Icmpv6Packet<_>>(IcmpParseArgs::new(src_ip, config.local_ip))
            .unwrap();
        ctx.receive_ndp_packet(
            device,
            src_ip.try_into().unwrap(),
            config.local_ip,
            icmpv6_packet.unwrap_ndp(),
        );
        let ndp_state = StateContext::<NdpState<EthernetLinkDevice>, _>::get_state_mut_with(
            &mut ctx,
            device.try_into().expect("expected ethernet ID"),
        );
        // Prefix should be in our list now.
        assert!(ndp_state.has_prefix(&subnet));
        // No new address should be formed.
        assert_empty(
            NdpContext::<EthernetLinkDevice>::get_ip_device_state(
                &ctx,
                device.try_into().expect("expected ethernet ID"),
            )
            .iter_global_ipv6_addrs(),
        );

        // Receive a new RA with new prefix (autonomous).
        //
        // Should get a new IP.

        let valid_lifetime = 10000;
        let preferred_lifetime = 9000;

        let mut icmpv6_packet_buf = slaac_packet_buf(
            src_ip,
            config.local_ip.get(),
            prefix,
            prefix_length,
            true,
            true,
            valid_lifetime,
            preferred_lifetime,
        );
        let icmpv6_packet = icmpv6_packet_buf
            .parse_with::<_, Icmpv6Packet<_>>(IcmpParseArgs::new(src_ip, config.local_ip))
            .unwrap();
        ctx.receive_ndp_packet(
            device,
            src_ip.try_into().unwrap(),
            config.local_ip,
            icmpv6_packet.unwrap_ndp(),
        );
        let ndp_state = StateContext::<NdpState<EthernetLinkDevice>, _>::get_state_mut_with(
            &mut ctx,
            device.try_into().expect("expected ethernet ID"),
        );
        assert!(ndp_state.has_prefix(&subnet));

        // Should have gotten a new IP.
        assert_eq!(
            NdpContext::<EthernetLinkDevice>::get_ip_device_state(
                &ctx,
                device.try_into().expect("expected ethernet ID")
            )
            .iter_global_ipv6_addrs()
            .count(),
            1
        );
        let entry = NdpContext::<EthernetLinkDevice>::get_ip_device_state(
            &ctx,
            device.try_into().expect("expected ethernet ID"),
        )
        .iter_global_ipv6_addrs()
        .last()
        .unwrap();
        assert_eq!(entry.addr_sub().subnet(), expected_subnet);
        assert_eq!(entry.state, AddressState::Tentative { dad_transmits_remaining: None });
        assert_eq!(entry.config_type(), AddrConfigType::Slaac);

        // Make sure deprecate and invalidation timers are set.
        let now = ctx.now();
        assert_eq!(
            ctx.dispatcher
                .timer_events()
                .filter(|x| (*x.0
                    == now.checked_add(Duration::from_secs(preferred_lifetime.into())).unwrap())
                    && (*x.1
                        == NdpTimerId::new_deprecate_slaac_address(
                            device.try_into().expect("expected ethernet ID"),
                            expected_addr
                        )
                        .into()))
                .count(),
            1
        );
        assert_eq!(
            ctx.dispatcher
                .timer_events()
                .filter(|x| (*x.0
                    == now.checked_add(Duration::from_secs(valid_lifetime.into())).unwrap())
                    && (*x.1
                        == NdpTimerId::new_invalidate_slaac_address(
                            device.try_into().expect("expected ethernet ID"),
                            expected_addr
                        )
                        .into()))
                .count(),
            1
        );

        // Complete DAD
        assert_eq!(
            run_for(&mut ctx, Duration::from_secs(1)),
            vec!(dad_timer_id(device.try_into().expect("expected ethernet ID"), expected_addr))
        );

        let entry = NdpContext::<EthernetLinkDevice>::get_ip_device_state(
            &ctx,
            device.try_into().expect("expected ethernet ID"),
        )
        .iter_global_ipv6_addrs()
        .last()
        .unwrap();
        assert_eq!(entry.addr_sub().subnet(), expected_subnet);
        assert_eq!(entry.state, AddressState::Assigned);
        assert_eq!(entry.config_type(), AddrConfigType::Slaac);

        // Receive the same RA.
        //
        // Should not get a new IP, but keep the one generated before.

        let mut icmpv6_packet_buf = slaac_packet_buf(
            src_ip,
            config.local_ip.get(),
            prefix,
            prefix_length,
            true,
            true,
            valid_lifetime,
            preferred_lifetime,
        );
        let icmpv6_packet = icmpv6_packet_buf
            .parse_with::<_, Icmpv6Packet<_>>(IcmpParseArgs::new(src_ip, config.local_ip))
            .unwrap();
        ctx.receive_ndp_packet(
            device,
            src_ip.try_into().unwrap(),
            config.local_ip,
            icmpv6_packet.unwrap_ndp(),
        );
        let ndp_state = StateContext::<NdpState<EthernetLinkDevice>, _>::get_state_mut_with(
            &mut ctx,
            device.try_into().expect("expected ethernet ID"),
        );
        assert!(ndp_state.has_prefix(&subnet));

        // Should not have changed.
        assert_eq!(
            NdpContext::<EthernetLinkDevice>::get_ip_device_state(
                &ctx,
                device.try_into().expect("expected ethernet ID")
            )
            .iter_global_ipv6_addrs()
            .count(),
            1
        );
        let entry = NdpContext::<EthernetLinkDevice>::get_ip_device_state(
            &ctx,
            device.try_into().expect("expected ethernet ID"),
        )
        .iter_global_ipv6_addrs()
        .last()
        .unwrap();
        assert_eq!(entry.addr_sub().subnet(), expected_subnet);
        assert_eq!(entry.state, AddressState::Assigned);
        assert_eq!(entry.config_type(), AddrConfigType::Slaac);

        // Timers should have been reset.
        let now = ctx.now();
        assert_eq!(
            ctx.dispatcher
                .timer_events()
                .filter(|x| (*x.0
                    == now.checked_add(Duration::from_secs(preferred_lifetime.into())).unwrap())
                    && (*x.1
                        == NdpTimerId::new_deprecate_slaac_address(
                            device.try_into().expect("expected ethernet ID"),
                            expected_addr
                        )
                        .into()))
                .count(),
            1
        );
        assert_eq!(
            ctx.dispatcher
                .timer_events()
                .filter(|x| (*x.0
                    == now.checked_add(Duration::from_secs(valid_lifetime.into())).unwrap())
                    && (*x.1
                        == NdpTimerId::new_invalidate_slaac_address(
                            device.try_into().expect("expected ethernet ID"),
                            expected_addr
                        )
                        .into()))
                .count(),
            1
        );

        //
        // Preferred lifetime expiration.
        //
        // Should be marked as deprecated.
        //
        assert_eq!(
            run_for(&mut ctx, Duration::from_secs(preferred_lifetime.into())),
            vec!(NdpTimerId::new_deprecate_slaac_address(
                device.try_into().expect("expected ethernet ID"),
                expected_addr
            )
            .into())
        );
        let entry = NdpContext::<EthernetLinkDevice>::get_ip_device_state(
            &ctx,
            device.try_into().expect("expected ethernet ID"),
        )
        .iter_global_ipv6_addrs()
        .last()
        .unwrap();
        assert_eq!(entry.state, AddressState::Deprecated);
        assert_eq!(entry.config_type(), AddrConfigType::Slaac);

        // Valid lifetime expiration.
        //
        // Should be deleted.

        assert_eq!(
            run_for(&mut ctx, Duration::from_secs((valid_lifetime - preferred_lifetime).into())),
            vec!(
                NdpTimerId::new_prefix_invalidation(
                    device.try_into().expect("expected ethernet ID"),
                    subnet
                )
                .into(),
                NdpTimerId::new_invalidate_slaac_address(
                    device.try_into().expect("expected ethernet ID"),
                    expected_addr
                )
                .into()
            )
        );
        assert_empty(
            NdpContext::<EthernetLinkDevice>::get_ip_device_state(
                &ctx,
                device.try_into().expect("expected ethernet ID"),
            )
            .iter_global_ipv6_addrs(),
        );

        // No more timers.
        assert_eq!(trigger_next_timer(&mut ctx), None);
    }

    #[test]
    fn test_host_stateless_address_autoconfiguration_new_ra_with_preferred_lifetime_zero() {
        let config = Ipv6::DUMMY_CONFIG;
        let mut ctx = DummyEventDispatcherBuilder::default().build::<DummyEventDispatcher>();
        let device = ctx.state.add_ethernet_device(config.local_mac, Ipv6::MINIMUM_LINK_MTU.into());
        crate::device::initialize_device(&mut ctx, device);

        let src_mac = config.remote_mac;
        let src_ip = src_mac.to_ipv6_link_local().addr().get();
        let prefix = Ipv6Addr::from([1, 2, 3, 4, 5, 6, 7, 8, 0, 0, 0, 0, 0, 0, 0, 0]);
        let prefix_length = 64;
        let subnet = Subnet::new(prefix, prefix_length).unwrap();
        let mut expected_addr = [1, 2, 3, 4, 5, 6, 7, 8, 0, 0, 0, 0, 0, 0, 0, 0];
        expected_addr[8..].copy_from_slice(&config.local_mac.to_eui64()[..]);
        let expected_addr = UnicastAddr::new(Ipv6Addr::from(expected_addr)).unwrap();

        // Enable DAD for future IPs.
        crate::device::set_ipv6_configuration(
            &mut ctx,
            device,
            crate::device::Ipv6DeviceConfiguration::default(),
        );

        // Receive a new RA with new prefix (autonomous).
        //
        // Should get a new IP.

        let valid_lifetime = 10000;

        let mut icmpv6_packet_buf = slaac_packet_buf(
            src_ip,
            config.local_ip.get(),
            prefix,
            prefix_length,
            true,
            true,
            valid_lifetime,
            0,
        );
        let icmpv6_packet = icmpv6_packet_buf
            .parse_with::<_, Icmpv6Packet<_>>(IcmpParseArgs::new(src_ip, config.local_ip))
            .unwrap();
        ctx.receive_ndp_packet(
            device,
            src_ip.try_into().unwrap(),
            config.local_ip,
            icmpv6_packet.unwrap_ndp(),
        );
        let ndp_state = StateContext::<NdpState<EthernetLinkDevice>, _>::get_state_mut_with(
            &mut ctx,
            device.try_into().expect("expected ethernet ID"),
        );
        assert!(ndp_state.has_prefix(&subnet));

        // Should NOT have gotten a new IP.
        assert_empty(
            NdpContext::<EthernetLinkDevice>::get_ip_device_state(
                &ctx,
                device.try_into().expect("expected ethernet ID"),
            )
            .iter_global_ipv6_addrs(),
        );

        // Make sure deprecate and invalidation timers are set.
        let now = ctx.now();
        assert_empty(ctx.dispatcher.timer_events().filter(|x| {
            *x.1 == NdpTimerId::new_deprecate_slaac_address(
                device.try_into().expect("expected ethernet ID"),
                expected_addr,
            )
            .into()
        }));
        assert_empty(ctx.dispatcher.timer_events().filter(|x| {
            *x.0 == now.checked_add(Duration::from_secs(valid_lifetime.into())).unwrap()
                && *x.1
                    == NdpTimerId::new_invalidate_slaac_address(
                        device.try_into().expect("expected ethernet ID"),
                        expected_addr,
                    )
                    .into()
        }));
        assert_empty(ctx.dispatcher.timer_events().filter(|x| {
            *x.1 == dad_timer_id(device.try_into().expect("expected ethernet ID"), expected_addr)
        }));

        assert_eq!(run_for(&mut ctx, Duration::from_secs(1)), []);

        assert_empty(
            NdpContext::<EthernetLinkDevice>::get_ip_device_state(
                &ctx,
                device.try_into().expect("expected ethernet ID"),
            )
            .iter_global_ipv6_addrs(),
        );
    }

    #[test]
    fn test_host_stateless_address_autoconfiguration_updated_ra_with_preferred_lifetime_zero() {
        let config = Ipv6::DUMMY_CONFIG;
        let mut ctx = DummyEventDispatcherBuilder::default().build::<DummyEventDispatcher>();
        let device = ctx.state.add_ethernet_device(config.local_mac, Ipv6::MINIMUM_LINK_MTU.into());
        crate::device::initialize_device(&mut ctx, device);

        let src_mac = config.remote_mac;
        let src_ip = src_mac.to_ipv6_link_local().addr().get();
        let prefix = Ipv6Addr::from([1, 2, 3, 4, 5, 6, 7, 8, 0, 0, 0, 0, 0, 0, 0, 0]);
        let prefix_length = 64;
        let subnet = Subnet::new(prefix, prefix_length).unwrap();
        let mut expected_addr = [1, 2, 3, 4, 5, 6, 7, 8, 0, 0, 0, 0, 0, 0, 0, 0];
        expected_addr[8..].copy_from_slice(&config.local_mac.to_eui64()[..]);
        let expected_addr = UnicastAddr::new(Ipv6Addr::from(expected_addr)).unwrap();
        let expected_subnet = Subnet::from_host(*expected_addr, prefix_length).unwrap();

        // Enable DAD for future IPs.
        crate::device::set_ipv6_configuration(
            &mut ctx,
            device,
            crate::device::Ipv6DeviceConfiguration::default(),
        );

        // Receive a new RA with new prefix (autonomous).
        //
        // Should get a new IP.

        let valid_lifetime = 10000;
        let preferred_lifetime = 9000;

        let mut icmpv6_packet_buf = slaac_packet_buf(
            src_ip,
            config.local_ip.get(),
            prefix,
            prefix_length,
            true,
            true,
            valid_lifetime,
            preferred_lifetime,
        );
        let icmpv6_packet = icmpv6_packet_buf
            .parse_with::<_, Icmpv6Packet<_>>(IcmpParseArgs::new(src_ip, config.local_ip))
            .unwrap();
        ctx.receive_ndp_packet(
            device,
            src_ip.try_into().unwrap(),
            config.local_ip,
            icmpv6_packet.unwrap_ndp(),
        );
        let ndp_state = StateContext::<NdpState<EthernetLinkDevice>, _>::get_state_mut_with(
            &mut ctx,
            device.try_into().expect("expected ethernet ID"),
        );
        assert!(ndp_state.has_prefix(&subnet));

        // Should have gotten a new IP.
        assert_eq!(
            NdpContext::<EthernetLinkDevice>::get_ip_device_state(
                &ctx,
                device.try_into().expect("expected ethernet ID")
            )
            .iter_global_ipv6_addrs()
            .count(),
            1
        );
        let entry = NdpContext::<EthernetLinkDevice>::get_ip_device_state(
            &ctx,
            device.try_into().expect("expected ethernet ID"),
        )
        .iter_global_ipv6_addrs()
        .last()
        .unwrap();
        assert_eq!(entry.addr_sub().subnet(), expected_subnet);
        assert_eq!(entry.state, AddressState::Tentative { dad_transmits_remaining: None });
        assert_eq!(entry.config_type(), AddrConfigType::Slaac);

        // Make sure deprecate and invalidation timers are set.
        let now = ctx.now();
        assert_eq!(
            ctx.dispatcher
                .timer_events()
                .filter(|x| (*x.0
                    == now.checked_add(Duration::from_secs(preferred_lifetime.into())).unwrap())
                    && (*x.1
                        == NdpTimerId::new_deprecate_slaac_address(
                            device.try_into().expect("expected ethernet ID"),
                            expected_addr
                        )
                        .into()))
                .count(),
            1
        );
        assert_eq!(
            ctx.dispatcher
                .timer_events()
                .filter(|x| (*x.0
                    == now.checked_add(Duration::from_secs(valid_lifetime.into())).unwrap())
                    && (*x.1
                        == NdpTimerId::new_invalidate_slaac_address(
                            device.try_into().expect("expected ethernet ID"),
                            expected_addr
                        )
                        .into()))
                .count(),
            1
        );
        assert_eq!(
            ctx.dispatcher
                .timer_events()
                .filter(|x| (*x.1
                    == dad_timer_id(
                        device.try_into().expect("expected ethernet ID"),
                        expected_addr
                    )))
                .count(),
            1
        );

        assert_eq!(
            run_for(&mut ctx, Duration::from_secs(1)),
            vec!(dad_timer_id(device.try_into().expect("expected ethernet ID"), expected_addr))
        );

        let entry = NdpContext::<EthernetLinkDevice>::get_ip_device_state(
            &ctx,
            device.try_into().expect("expected ethernet ID"),
        )
        .iter_global_ipv6_addrs()
        .last()
        .unwrap();
        assert_eq!(entry.addr_sub().subnet(), expected_subnet);
        assert_eq!(entry.state, AddressState::Assigned);
        assert_eq!(entry.config_type(), AddrConfigType::Slaac);

        //
        // Receive the same RA, now with preferred_lifetime = 0
        //
        // Should not get a new IP, but keep the one generated before.
        //
        let mut icmpv6_packet_buf = slaac_packet_buf(
            src_ip,
            config.local_ip.get(),
            prefix,
            prefix_length,
            true,
            true,
            valid_lifetime,
            0,
        );
        let icmpv6_packet = icmpv6_packet_buf
            .parse_with::<_, Icmpv6Packet<_>>(IcmpParseArgs::new(src_ip, config.local_ip))
            .unwrap();
        ctx.receive_ndp_packet(
            device,
            src_ip.try_into().unwrap(),
            config.local_ip,
            icmpv6_packet.unwrap_ndp(),
        );
        let ndp_state = StateContext::<NdpState<EthernetLinkDevice>, _>::get_state_mut_with(
            &mut ctx,
            device.try_into().expect("expected ethernet ID"),
        );
        assert!(ndp_state.has_prefix(&subnet));

        // Should not have changed.
        assert_eq!(
            NdpContext::<EthernetLinkDevice>::get_ip_device_state(
                &ctx,
                device.try_into().expect("expected ethernet ID")
            )
            .iter_global_ipv6_addrs()
            .count(),
            1
        );
        let entry = NdpContext::<EthernetLinkDevice>::get_ip_device_state(
            &ctx,
            device.try_into().expect("expected ethernet ID"),
        )
        .iter_global_ipv6_addrs()
        .last()
        .unwrap();
        assert_eq!(entry.addr_sub().subnet(), expected_subnet);
        assert_eq!(entry.state, AddressState::Deprecated);
        assert_eq!(entry.config_type(), AddrConfigType::Slaac);

        // Timers should have been reset.
        let now = ctx.now();
        assert_empty(ctx.dispatcher.timer_events().filter(|x| {
            *x.1 == NdpTimerId::new_deprecate_slaac_address(
                device.try_into().expect("expected ethernet ID"),
                expected_addr,
            )
            .into()
        }));
        assert_eq!(
            ctx.dispatcher
                .timer_events()
                .filter(|x| (*x.0
                    == now.checked_add(Duration::from_secs(valid_lifetime.into())).unwrap())
                    && *x.1
                        == NdpTimerId::new_invalidate_slaac_address(
                            device.try_into().expect("expected ethernet ID"),
                            expected_addr
                        )
                        .into())
                .count(),
            1
        );

        //
        // Receive the same RA (again), still with preferred_lifetime = 0
        //
        // Should not get a new IP, but keep the one generated before.
        //
        let mut icmpv6_packet_buf = slaac_packet_buf(
            src_ip,
            config.local_ip.get(),
            prefix,
            prefix_length,
            true,
            true,
            valid_lifetime,
            0,
        );
        let icmpv6_packet = icmpv6_packet_buf
            .parse_with::<_, Icmpv6Packet<_>>(IcmpParseArgs::new(src_ip, config.local_ip))
            .unwrap();
        ctx.receive_ndp_packet(
            device,
            src_ip.try_into().unwrap(),
            config.local_ip,
            icmpv6_packet.unwrap_ndp(),
        );
        let ndp_state = StateContext::<NdpState<EthernetLinkDevice>, _>::get_state_mut_with(
            &mut ctx,
            device.try_into().expect("expected ethernet ID"),
        );
        assert!(ndp_state.has_prefix(&subnet));

        // Should not have changed.
        assert_eq!(
            NdpContext::<EthernetLinkDevice>::get_ip_device_state(
                &ctx,
                device.try_into().expect("expected ethernet ID")
            )
            .iter_global_ipv6_addrs()
            .count(),
            1
        );
        let entry = NdpContext::<EthernetLinkDevice>::get_ip_device_state(
            &ctx,
            device.try_into().expect("expected ethernet ID"),
        )
        .iter_global_ipv6_addrs()
        .last()
        .unwrap();
        assert_eq!(entry.addr_sub().subnet(), expected_subnet);
        assert_eq!(entry.state, AddressState::Deprecated);
        assert_eq!(entry.config_type(), AddrConfigType::Slaac);

        // Timers should have been reset.
        let now = ctx.now();
        assert_empty(ctx.dispatcher.timer_events().filter(|x| {
            *x.1 == NdpTimerId::new_deprecate_slaac_address(
                device.try_into().expect("expected ethernet ID"),
                expected_addr,
            )
            .into()
        }));
        assert_eq!(
            ctx.dispatcher
                .timer_events()
                .filter(|x| (*x.0
                    == now.checked_add(Duration::from_secs(valid_lifetime.into())).unwrap())
                    && *x.1
                        == NdpTimerId::new_invalidate_slaac_address(
                            device.try_into().expect("expected ethernet ID"),
                            expected_addr
                        )
                        .into())
                .count(),
            1
        );

        //
        // Receive the same RA, now with preferred_lifetime > 0
        //
        // Should not get a new IP, but keep the one generated before.
        //
        let mut icmpv6_packet_buf = slaac_packet_buf(
            src_ip,
            config.local_ip.get(),
            prefix,
            prefix_length,
            true,
            true,
            valid_lifetime,
            preferred_lifetime,
        );
        let icmpv6_packet = icmpv6_packet_buf
            .parse_with::<_, Icmpv6Packet<_>>(IcmpParseArgs::new(src_ip, config.local_ip))
            .unwrap();
        ctx.receive_ndp_packet(
            device,
            src_ip.try_into().unwrap(),
            config.local_ip,
            icmpv6_packet.unwrap_ndp(),
        );
        let ndp_state = StateContext::<NdpState<EthernetLinkDevice>, _>::get_state_mut_with(
            &mut ctx,
            device.try_into().expect("expected ethernet ID"),
        );
        assert!(ndp_state.has_prefix(&subnet));

        // Should not have changed.
        assert_eq!(
            NdpContext::<EthernetLinkDevice>::get_ip_device_state(
                &ctx,
                device.try_into().expect("expected ethernet ID")
            )
            .iter_global_ipv6_addrs()
            .count(),
            1
        );
        let entry = NdpContext::<EthernetLinkDevice>::get_ip_device_state(
            &ctx,
            device.try_into().expect("expected ethernet ID"),
        )
        .iter_global_ipv6_addrs()
        .last()
        .unwrap();
        assert_eq!(entry.addr_sub().subnet(), expected_subnet);
        assert_eq!(entry.state, AddressState::Assigned);
        assert_eq!(entry.config_type(), AddrConfigType::Slaac);

        // Make sure deprecate and invalidation timers are set.
        let now = ctx.now();
        assert_eq!(
            ctx.dispatcher
                .timer_events()
                .filter(|x| (*x.0
                    == now.checked_add(Duration::from_secs(preferred_lifetime.into())).unwrap())
                    && (*x.1
                        == NdpTimerId::new_deprecate_slaac_address(
                            device.try_into().expect("expected ethernet ID"),
                            expected_addr
                        )
                        .into()))
                .count(),
            1
        );
        assert_eq!(
            ctx.dispatcher
                .timer_events()
                .filter(|x| (*x.0
                    == now.checked_add(Duration::from_secs(valid_lifetime.into())).unwrap())
                    && (*x.1
                        == NdpTimerId::new_invalidate_slaac_address(
                            device.try_into().expect("expected ethernet ID"),
                            expected_addr
                        )
                        .into()))
                .count(),
            1
        );
        assert_empty(ctx.dispatcher.timer_events().filter(|x| {
            *x.1 == dad_timer_id(device.try_into().expect("expected ethernet ID"), expected_addr)
        }));

        assert_eq!(run_for(&mut ctx, Duration::from_secs(1)), vec!());

        let entry = NdpContext::<EthernetLinkDevice>::get_ip_device_state(
            &ctx,
            device.try_into().expect("expected ethernet ID"),
        )
        .iter_global_ipv6_addrs()
        .last()
        .unwrap();
        assert_eq!(entry.addr_sub().subnet(), expected_subnet);
        assert_eq!(entry.state, AddressState::Assigned);
        assert_eq!(entry.config_type(), AddrConfigType::Slaac);

        //
        // Preferred lifetime expiration.
        //
        // Should be marked as deprecated.
        //
        assert_eq!(
            run_for(&mut ctx, Duration::from_secs(preferred_lifetime.into())),
            vec!(NdpTimerId::new_deprecate_slaac_address(
                device.try_into().expect("expected ethernet ID"),
                expected_addr
            )
            .into())
        );

        let entry = NdpContext::<EthernetLinkDevice>::get_ip_device_state(
            &ctx,
            device.try_into().expect("expected ethernet ID"),
        )
        .iter_global_ipv6_addrs()
        .last()
        .unwrap();
        assert_eq!(entry.state, AddressState::Deprecated);
        assert_eq!(entry.config_type(), AddrConfigType::Slaac);

        //
        // Valid lifetime expiration.
        //
        // Should be deleted.
        //
        assert_eq!(
            run_for(&mut ctx, Duration::from_secs((valid_lifetime - preferred_lifetime).into())),
            vec!(
                NdpTimerId::new_prefix_invalidation(
                    device.try_into().expect("expected ethernet ID"),
                    subnet
                )
                .into(),
                NdpTimerId::new_invalidate_slaac_address(
                    device.try_into().expect("expected ethernet ID"),
                    expected_addr
                )
                .into()
            )
        );
        assert_empty(
            NdpContext::<EthernetLinkDevice>::get_ip_device_state(
                &ctx,
                device.try_into().expect("expected ethernet ID"),
            )
            .iter_global_ipv6_addrs(),
        );

        // No more timers.
        assert_eq!(trigger_next_timer(&mut ctx), None);
    }

    #[test]
    fn test_host_slaac_and_manual_address_and_prefix_conflict() {
        // SLAAC should not overwrite any manually added addresses that may
        // conflict with the generated SLAAC address.

        let config = Ipv6::DUMMY_CONFIG;
        let mut ctx = DummyEventDispatcherBuilder::default().build::<DummyEventDispatcher>();
        let device = ctx.state.add_ethernet_device(config.local_mac, Ipv6::MINIMUM_LINK_MTU.into());
        crate::device::initialize_device(&mut ctx, device);

        let src_mac = config.remote_mac;
        let src_ip = src_mac.to_ipv6_link_local().addr().get();
        let prefix = Ipv6Addr::from([1, 2, 3, 4, 5, 6, 7, 8, 0, 0, 0, 0, 0, 0, 0, 0]);
        let prefix_length = 64;
        let mut expected_addr = [1, 2, 3, 4, 5, 6, 7, 8, 0, 0, 0, 0, 0, 0, 0, 0];
        expected_addr[8..].copy_from_slice(&config.local_mac.to_eui64()[..]);
        let expected_addr = UnicastAddr::new(Ipv6Addr::from(expected_addr)).unwrap();
        let expected_addr_sub = AddrSubnet::from_witness(expected_addr, prefix_length).unwrap();

        // Manually give the device the expected address/subnet
        add_ip_addr_subnet(&mut ctx, device, expected_addr_sub.to_witness()).unwrap();
        assert_eq!(
            NdpContext::<EthernetLinkDevice>::get_ip_device_state(
                &ctx,
                device.try_into().expect("expected ethernet ID")
            )
            .iter_global_ipv6_addrs()
            .count(),
            1
        );
        let entry = NdpContext::<EthernetLinkDevice>::get_ip_device_state(
            &ctx,
            device.try_into().expect("expected ethernet ID"),
        )
        .iter_global_ipv6_addrs()
        .last()
        .unwrap();
        assert_eq!(entry.state, AddressState::Assigned);
        assert_eq!(entry.config_type(), AddrConfigType::Manual);
        assert_empty(ctx.dispatcher.timer_events());

        // Receive a new RA with new prefix (autonomous).
        //
        // Should not get a new IP.

        let mut icmpv6_packet_buf = slaac_packet_buf(
            src_ip,
            config.local_ip.get(),
            prefix,
            prefix_length,
            false,
            true,
            10000,
            9000,
        );
        let icmpv6_packet = icmpv6_packet_buf
            .parse_with::<_, Icmpv6Packet<_>>(IcmpParseArgs::new(src_ip, config.local_ip))
            .unwrap();
        ctx.receive_ndp_packet(
            device,
            src_ip.try_into().unwrap(),
            config.local_ip,
            icmpv6_packet.unwrap_ndp(),
        );

        // Address state and configuration type should not have changed.
        assert_eq!(
            NdpContext::<EthernetLinkDevice>::get_ip_device_state(
                &ctx,
                device.try_into().expect("expected ethernet ID")
            )
            .iter_global_ipv6_addrs()
            .count(),
            1
        );
        let entry = NdpContext::<EthernetLinkDevice>::get_ip_device_state(
            &ctx,
            device.try_into().expect("expected ethernet ID"),
        )
        .iter_global_ipv6_addrs()
        .last()
        .unwrap();
        assert_eq!(*entry.addr_sub(), expected_addr_sub);
        assert_eq!(entry.state, AddressState::Assigned);
        assert_eq!(entry.config_type(), AddrConfigType::Manual);

        // No new timers were added.
        assert_empty(ctx.dispatcher.timer_events());
    }

    #[test]
    fn test_host_slaac_and_manual_address_conflict() {
        // SLAAC should not overwrite any manually added addresses that may
        // conflict with the generated SLAAC address, even if the manually added
        // address has a different prefix.

        let config = Ipv6::DUMMY_CONFIG;
        let mut ctx = DummyEventDispatcherBuilder::default().build::<DummyEventDispatcher>();
        let device = ctx.state.add_ethernet_device(config.local_mac, Ipv6::MINIMUM_LINK_MTU.into());
        crate::device::initialize_device(&mut ctx, device);

        let src_mac = config.remote_mac;
        let src_ip = src_mac.to_ipv6_link_local().addr().get();
        let prefix = Ipv6Addr::from([1, 2, 3, 4, 5, 6, 7, 8, 0, 0, 0, 0, 0, 0, 0, 0]);
        let prefix_length = 64;
        let mut manual_addr_sub = [1, 2, 3, 4, 5, 6, 7, 8, 0, 0, 0, 0, 0, 0, 0, 0];
        manual_addr_sub[8..].copy_from_slice(&config.local_mac.to_eui64()[..]);
        let manual_addr_sub = UnicastAddr::new(Ipv6Addr::from(manual_addr_sub)).unwrap();
        let manual_addr_sub =
            AddrSubnet::from_witness(manual_addr_sub, prefix_length + 10).unwrap();
        // Manually give the device the expected address but with a different
        // prefix.
        add_ip_addr_subnet(&mut ctx, device, manual_addr_sub.to_witness()).unwrap();
        assert_eq!(
            NdpContext::<EthernetLinkDevice>::get_ip_device_state(
                &ctx,
                device.try_into().expect("expected ethernet ID")
            )
            .iter_global_ipv6_addrs()
            .count(),
            1
        );
        let entry = NdpContext::<EthernetLinkDevice>::get_ip_device_state(
            &ctx,
            device.try_into().expect("expected ethernet ID"),
        )
        .iter_global_ipv6_addrs()
        .last()
        .unwrap();
        assert_eq!(*entry.addr_sub(), manual_addr_sub);
        assert_eq!(entry.state, AddressState::Assigned);
        assert_eq!(entry.config_type(), AddrConfigType::Manual);
        assert_empty(ctx.dispatcher.timer_events());

        // Receive a new RA with new prefix (autonomous).
        //
        // Should not get a new IP.

        let mut icmpv6_packet_buf = slaac_packet_buf(
            src_ip,
            config.local_ip.get(),
            prefix,
            prefix_length,
            false,
            true,
            10000,
            9000,
        );
        let icmpv6_packet = icmpv6_packet_buf
            .parse_with::<_, Icmpv6Packet<_>>(IcmpParseArgs::new(src_ip, config.local_ip))
            .unwrap();
        ctx.receive_ndp_packet(
            device,
            src_ip.try_into().unwrap(),
            config.local_ip,
            icmpv6_packet.unwrap_ndp(),
        );
        assert_eq!(
            NdpContext::<EthernetLinkDevice>::get_ip_device_state(
                &ctx,
                device.try_into().expect("expected ethernet ID")
            )
            .iter_global_ipv6_addrs()
            .count(),
            1
        );
        let entry = NdpContext::<EthernetLinkDevice>::get_ip_device_state(
            &ctx,
            device.try_into().expect("expected ethernet ID"),
        )
        .iter_global_ipv6_addrs()
        .last()
        .unwrap();
        // Address state and configuration type should not have changed.
        assert_eq!(*entry.addr_sub(), manual_addr_sub);
        assert_eq!(entry.state, AddressState::Assigned);
        assert_eq!(entry.config_type(), AddrConfigType::Manual);

        // No new timers were added.
        assert_empty(ctx.dispatcher.timer_events());
    }

    #[test]
    fn test_host_slaac_and_manual_address_prefix_conflict() {
        // SLAAC should not overwrite any manually added addresses that use the
        // same prefix as a SLAAC generated one.

        let config = Ipv6::DUMMY_CONFIG;
        let mut ctx = DummyEventDispatcherBuilder::default().build::<DummyEventDispatcher>();
        let device = ctx.state.add_ethernet_device(config.local_mac, Ipv6::MINIMUM_LINK_MTU.into());
        crate::device::initialize_device(&mut ctx, device);

        let src_mac = config.remote_mac;
        let src_ip = src_mac.to_ipv6_link_local().addr().get();
        let prefix = Ipv6Addr::from([1, 2, 3, 4, 5, 6, 7, 8, 0, 0, 0, 0, 0, 0, 0, 0]);
        let prefix_length = 64;
        let manual_addr = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
        let manual_addr = UnicastAddr::new(Ipv6Addr::from(manual_addr)).unwrap();
        let manual_addr_sub = AddrSubnet::from_witness(manual_addr, prefix_length).unwrap();
        let mut slaac_addr = [1, 2, 3, 4, 5, 6, 7, 8, 0, 0, 0, 0, 0, 0, 0, 0];
        slaac_addr[8..].copy_from_slice(&config.local_mac.to_eui64());
        let slaac_addr = Ipv6Addr::from(slaac_addr);
        let slaac_addr_sub = AddrSubnet::new(slaac_addr, prefix_length).unwrap();
        // Manually give the device the expected address with the same prefix.
        add_ip_addr_subnet(&mut ctx, device, manual_addr_sub.to_witness()).unwrap();
        assert_eq!(
            NdpContext::<EthernetLinkDevice>::get_ip_device_state(
                &ctx,
                device.try_into().expect("expected ethernet ID")
            )
            .iter_global_ipv6_addrs()
            .count(),
            1
        );
        let entry = NdpContext::<EthernetLinkDevice>::get_ip_device_state(
            &ctx,
            device.try_into().expect("expected ethernet ID"),
        )
        .iter_global_ipv6_addrs()
        .last()
        .unwrap();
        assert_eq!(entry.state, AddressState::Assigned);
        assert_eq!(entry.config_type(), AddrConfigType::Manual);
        assert_empty(ctx.dispatcher.timer_events());

        // Receive a new RA with new prefix (autonomous).
        //
        // Should not get a new IP.

        let mut icmpv6_packet_buf = slaac_packet_buf(
            src_ip,
            config.local_ip.get(),
            prefix,
            prefix_length,
            false,
            true,
            10000,
            9000,
        );
        let icmpv6_packet = icmpv6_packet_buf
            .parse_with::<_, Icmpv6Packet<_>>(IcmpParseArgs::new(src_ip, config.local_ip))
            .unwrap();
        ctx.receive_ndp_packet(
            device,
            src_ip.try_into().unwrap(),
            config.local_ip,
            icmpv6_packet.unwrap_ndp(),
        );
        assert_eq!(
            NdpContext::<EthernetLinkDevice>::get_ip_device_state(
                &ctx,
                device.try_into().expect("expected ethernet ID")
            )
            .iter_global_ipv6_addrs()
            .count(),
            2
        );
        let entry = NdpContext::<EthernetLinkDevice>::get_ip_device_state(
            &ctx,
            device.try_into().expect("expected ethernet ID"),
        )
        .iter_global_ipv6_addrs()
        .nth(0)
        .unwrap();
        // Address state and configuration type should not have changed.
        assert_eq!(*entry.addr_sub(), manual_addr_sub);
        assert_eq!(entry.state, AddressState::Assigned);
        assert_eq!(entry.config_type(), AddrConfigType::Manual);
        let entry = NdpContext::<EthernetLinkDevice>::get_ip_device_state(
            &ctx,
            device.try_into().expect("expected ethernet ID"),
        )
        .iter_global_ipv6_addrs()
        .last()
        .unwrap();
        // Address state and configuration type should not have changed.
        assert_eq!(*entry.addr_sub(), slaac_addr_sub);
        assert_eq!(entry.state, AddressState::Assigned);
        assert_eq!(entry.config_type(), AddrConfigType::Slaac);

        // Address invalidation timers were added.
        assert_eq!(ctx.dispatcher.timer_events().count(), 2);
    }

    #[test]
    fn test_host_slaac_invalid_prefix_information() {
        let config = Ipv6::DUMMY_CONFIG;
        let mut ctx = DummyEventDispatcherBuilder::default().build::<DummyEventDispatcher>();
        let device = ctx.state.add_ethernet_device(config.local_mac, Ipv6::MINIMUM_LINK_MTU.into());
        crate::device::initialize_device(&mut ctx, device);

        let src_mac = config.remote_mac;
        let src_ip = src_mac.to_ipv6_link_local().addr().get();
        let prefix = Ipv6Addr::from([1, 2, 3, 4, 5, 6, 7, 8, 0, 0, 0, 0, 0, 0, 0, 0]);
        let prefix_length = 64;

        assert_empty(
            NdpContext::<EthernetLinkDevice>::get_ip_device_state(
                &ctx,
                device.try_into().expect("expected ethernet ID"),
            )
            .iter_global_ipv6_addrs(),
        );

        // Receive a new RA with new prefix (autonomous), but preferred lifetime
        // is greater than valid.
        //
        // Should not get a new IP.

        let mut icmpv6_packet_buf = slaac_packet_buf(
            src_ip,
            config.local_ip.get(),
            prefix,
            prefix_length,
            false,
            true,
            9000,
            10000,
        );
        let icmpv6_packet = icmpv6_packet_buf
            .parse_with::<_, Icmpv6Packet<_>>(IcmpParseArgs::new(src_ip, config.local_ip))
            .unwrap();
        ctx.receive_ndp_packet(
            device,
            src_ip.try_into().unwrap(),
            config.local_ip,
            icmpv6_packet.unwrap_ndp(),
        );
        assert_empty(
            NdpContext::<EthernetLinkDevice>::get_ip_device_state(
                &ctx,
                device.try_into().expect("expected ethernet ID"),
            )
            .iter_global_ipv6_addrs(),
        );

        // Address invalidation timers were added.
        assert_empty(ctx.dispatcher.timer_events());
    }

    #[test]
    fn test_host_slaac_address_deprecate_while_tentative() {
        // Invalidate an address right away if we attempt to deprecate a
        // tentative address.

        let config = Ipv6::DUMMY_CONFIG;
        let mut ctx = DummyEventDispatcherBuilder::default().build::<DummyEventDispatcher>();
        let device = ctx.state.add_ethernet_device(config.local_mac, Ipv6::MINIMUM_LINK_MTU.into());
        crate::device::initialize_device(&mut ctx, device);

        let src_mac = config.remote_mac;
        let src_ip = src_mac.to_ipv6_link_local().addr().get();
        let prefix = Ipv6Addr::from([1, 2, 3, 4, 5, 6, 7, 8, 0, 0, 0, 0, 0, 0, 0, 0]);
        let prefix_length = 64;
        let mut expected_addr = [1, 2, 3, 4, 5, 6, 7, 8, 0, 0, 0, 0, 0, 0, 0, 0];
        expected_addr[8..].copy_from_slice(&config.local_mac.to_eui64()[..]);
        let expected_addr = UnicastAddr::new(Ipv6Addr::from(expected_addr)).unwrap();
        let expected_addr_sub = AddrSubnet::from_witness(expected_addr, prefix_length).unwrap();

        // Have no addresses yet.
        assert_empty(
            NdpContext::<EthernetLinkDevice>::get_ip_device_state(
                &ctx,
                device.try_into().expect("expected ethernet ID"),
            )
            .iter_global_ipv6_addrs(),
        );

        // Enable DAD for future IPs.
        crate::device::set_ipv6_configuration(
            &mut ctx,
            device,
            crate::device::Ipv6DeviceConfiguration::default(),
        );

        // Set the retransmit timer between neighbor solicitations to be greater
        // than the preferred lifetime of the prefix.
        StateContext::<NdpState<EthernetLinkDevice>, _>::get_state_mut_with(
            &mut ctx,
            device.try_into().expect("expected ethernet ID"),
        )
        .set_retrans_timer(Duration::from_secs(10));

        // Receive a new RA with new prefix (autonomous).
        //
        // Should get a new IP and set preferred lifetime to 1s.

        let valid_lifetime = 2;
        let preferred_lifetime = 1;

        let mut icmpv6_packet_buf = slaac_packet_buf(
            src_ip,
            config.local_ip.get(),
            prefix,
            prefix_length,
            false,
            true,
            valid_lifetime,
            preferred_lifetime,
        );
        let icmpv6_packet = icmpv6_packet_buf
            .parse_with::<_, Icmpv6Packet<_>>(IcmpParseArgs::new(src_ip, config.local_ip))
            .unwrap();
        ctx.receive_ndp_packet(
            device,
            src_ip.try_into().unwrap(),
            config.local_ip,
            icmpv6_packet.unwrap_ndp(),
        );

        // Should have gotten a new IP.
        assert_eq!(
            NdpContext::<EthernetLinkDevice>::get_ip_device_state(
                &ctx,
                device.try_into().expect("expected ethernet ID")
            )
            .iter_global_ipv6_addrs()
            .count(),
            1
        );
        let entry = NdpContext::<EthernetLinkDevice>::get_ip_device_state(
            &ctx,
            device.try_into().expect("expected ethernet ID"),
        )
        .iter_global_ipv6_addrs()
        .last()
        .unwrap();
        assert_eq!(*entry.addr_sub(), expected_addr_sub);
        assert_eq!(entry.state, AddressState::Tentative { dad_transmits_remaining: None });
        assert_eq!(entry.config_type(), AddrConfigType::Slaac);

        // Make sure deprecate and invalidation timers are set.
        let now = ctx.now();
        assert_eq!(
            ctx.dispatcher
                .timer_events()
                .filter(|x| (*x.0
                    == now.checked_add(Duration::from_secs(preferred_lifetime.into())).unwrap())
                    && (*x.1
                        == NdpTimerId::new_deprecate_slaac_address(
                            device.try_into().expect("expected ethernet ID"),
                            expected_addr
                        )
                        .into()))
                .count(),
            1
        );
        assert_eq!(
            ctx.dispatcher
                .timer_events()
                .filter(|x| (*x.0
                    == now.checked_add(Duration::from_secs(valid_lifetime.into())).unwrap())
                    && (*x.1
                        == NdpTimerId::new_invalidate_slaac_address(
                            device.try_into().expect("expected ethernet ID"),
                            expected_addr
                        )
                        .into()))
                .count(),
            1
        );

        // Trigger the deprecation timer.
        assert_eq!(
            trigger_next_timer(&mut ctx).unwrap(),
            NdpTimerId::new_deprecate_slaac_address(
                device.try_into().expect("expected ethernet ID"),
                expected_addr
            )
            .into()
        );

        // At this point, the address (that was tentative) should just be
        // invalidated (removed) since we should not have any existing
        // connections using the tentative address.
        assert_empty(
            NdpContext::<EthernetLinkDevice>::get_ip_device_state(
                &ctx,
                device.try_into().expect("expected ethernet ID"),
            )
            .iter_global_ipv6_addrs(),
        );

        // No more timers.
        assert_eq!(trigger_next_timer(&mut ctx), None);
    }

    fn receive_prefix_update(
        ctx: &mut Ctx<DummyEventDispatcher>,
        device: DeviceId,
        src_ip: Ipv6Addr,
        dst_ip: SpecifiedAddr<Ipv6Addr>,
        subnet: Subnet<Ipv6Addr>,
        preferred_lifetime: u32,
        valid_lifetime: u32,
    ) {
        let prefix = subnet.network();
        let prefix_length = subnet.prefix();

        let mut icmpv6_packet_buf = slaac_packet_buf(
            src_ip,
            dst_ip.get(),
            prefix,
            prefix_length,
            false,
            true,
            valid_lifetime,
            preferred_lifetime,
        );
        let icmpv6_packet = icmpv6_packet_buf
            .parse_with::<_, Icmpv6Packet<_>>(IcmpParseArgs::new(src_ip, dst_ip))
            .unwrap();
        ctx.receive_ndp_packet(
            device,
            src_ip.try_into().unwrap(),
            dst_ip,
            icmpv6_packet.unwrap_ndp(),
        );
    }

    fn get_slaac_address_entry(
        ctx: &Ctx<DummyEventDispatcher>,
        device: EthernetDeviceId,
        addr_sub: AddrSubnet<Ipv6Addr, UnicastAddr<Ipv6Addr>>,
    ) -> Option<&Ipv6AddressEntry<DummyInstant>> {
        let mut matching_addrs = NdpContext::<EthernetLinkDevice>::get_ip_device_state(ctx, device)
            .iter_global_ipv6_addrs()
            .filter(|entry| *entry.addr_sub() == addr_sub);
        let entry = matching_addrs.next();
        assert_eq!(matching_addrs.next(), None);
        entry
    }

    fn assert_slaac_lifetimes_enforced(
        ctx: &Ctx<DummyEventDispatcher>,
        device: EthernetDeviceId,
        entry: &Ipv6AddressEntry<DummyInstant>,
        valid_until: DummyInstant,
        preferred_until: DummyInstant,
    ) {
        assert_eq!(entry.state, AddressState::Assigned);
        assert_eq!(entry.config_type(), AddrConfigType::Slaac);
        assert_eq!(entry.valid_until(), Some(valid_until));
        assert_eq!(
            ctx.dispatcher
                .timer_events()
                .filter_map(|(time, timer_id)| {
                    if *timer_id
                        == NdpTimerId::new_deprecate_slaac_address(device, entry.addr_sub().addr())
                            .into()
                    {
                        Some(*time)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>(),
            vec![preferred_until]
        );
        assert_eq!(
            ctx.dispatcher
                .timer_events()
                .filter_map(|(time, timer_id)| {
                    if *timer_id
                        == NdpTimerId::new_invalidate_slaac_address(device, entry.addr_sub().addr())
                            .into()
                    {
                        Some(*time)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>(),
            vec![valid_until]
        );
    }

    #[test]
    fn test_host_slaac_valid_lifetime_updates() {
        // Make sure we update the valid lifetime only in certain scenarios
        // to prevent denial-of-service attacks as outlined in RFC 4862 section
        // 5.5.3.e. Note, the preferred lifetime should always be updated.

        fn inner_test(
            ctx: &mut Ctx<DummyEventDispatcher>,
            device: DeviceId,
            src_ip: Ipv6Addr,
            dst_ip: SpecifiedAddr<Ipv6Addr>,
            subnet: Subnet<Ipv6Addr>,
            addr_sub: AddrSubnet<Ipv6Addr, UnicastAddr<Ipv6Addr>>,
            preferred_lifetime: u32,
            valid_lifetime: u32,
            expected_valid_lifetime: u32,
        ) {
            receive_prefix_update(
                ctx,
                device,
                src_ip,
                dst_ip,
                subnet,
                preferred_lifetime,
                valid_lifetime,
            );
            let entry = get_slaac_address_entry(
                ctx,
                device.try_into().expect("expected ethernet ID"),
                addr_sub,
            )
            .unwrap();
            let now = ctx.now();
            let valid_until =
                now.checked_add(Duration::from_secs(expected_valid_lifetime.into())).unwrap();
            let preferred_until =
                now.checked_add(Duration::from_secs(preferred_lifetime.into())).unwrap();

            assert_slaac_lifetimes_enforced(
                ctx,
                device.try_into().expect("expected ethernet ID"),
                entry,
                valid_until,
                preferred_until,
            );
        }

        let config = Ipv6::DUMMY_CONFIG;
        let mut ctx = DummyEventDispatcherBuilder::default().build::<DummyEventDispatcher>();
        let device = ctx.state.add_ethernet_device(config.local_mac, Ipv6::MINIMUM_LINK_MTU.into());
        crate::device::initialize_device(&mut ctx, device);

        let src_mac = config.remote_mac;
        let src_ip = src_mac.to_ipv6_link_local().addr().get();
        let dst_ip = config.local_ip;
        let prefix = Ipv6Addr::from([1, 2, 3, 4, 5, 6, 7, 8, 0, 0, 0, 0, 0, 0, 0, 0]);
        let prefix_length = 64;
        let subnet = Subnet::new(prefix, prefix_length).unwrap();
        let mut expected_addr = [1, 2, 3, 4, 5, 6, 7, 8, 0, 0, 0, 0, 0, 0, 0, 0];
        expected_addr[8..].copy_from_slice(&config.local_mac.to_eui64()[..]);
        let expected_addr = UnicastAddr::new(Ipv6Addr::from(expected_addr)).unwrap();
        let expected_addr_sub = AddrSubnet::from_witness(expected_addr, prefix_length).unwrap();

        // Have no addresses yet.
        assert_empty(
            NdpContext::<EthernetLinkDevice>::get_ip_device_state(
                &ctx,
                device.try_into().expect("expected ethernet ID"),
            )
            .iter_global_ipv6_addrs(),
        );

        // Receive a new RA with new prefix (autonomous).
        //
        // Should get a new IP and set preferred lifetime to 1s.

        // Make sure deprecate and invalidation timers are set.
        inner_test(&mut ctx, device, src_ip, dst_ip, subnet, expected_addr_sub, 30, 60, 60);

        // If the valid lifetime is greater than the remaining lifetime, update
        // the valid lifetime.
        inner_test(&mut ctx, device, src_ip, dst_ip, subnet, expected_addr_sub, 70, 70, 70);

        // If the valid lifetime is greater than 2 hrs, update the valid
        // lifetime.
        inner_test(&mut ctx, device, src_ip, dst_ip, subnet, expected_addr_sub, 1001, 7201, 7201);

        // Make remaining lifetime < 2 hrs.
        assert_eq!(run_for(&mut ctx, Duration::from_secs(1000)), []);

        // If the remaining lifetime is <= 2 hrs & valid lifetime is less than
        // that, don't update valid lifetime.
        inner_test(&mut ctx, device, src_ip, dst_ip, subnet, expected_addr_sub, 1000, 2000, 6201);

        // Make the remaining lifetime > 2 hours.
        inner_test(&mut ctx, device, src_ip, dst_ip, subnet, expected_addr_sub, 1000, 10800, 10800);

        // If the remaining lifetime is > 2 hours, and new valid lifetime is < 2
        // hours, set the valid lifetime to 2 hours.
        inner_test(&mut ctx, device, src_ip, dst_ip, subnet, expected_addr_sub, 1000, 1000, 7200);

        // If the remaining lifetime is <= 2 hrs & valid lifetime is less than
        // that, don't update valid lifetime.
        inner_test(&mut ctx, device, src_ip, dst_ip, subnet, expected_addr_sub, 1000, 2000, 7200);

        // Increase valid lifetime twice while it is greater than 2 hours.
        inner_test(&mut ctx, device, src_ip, dst_ip, subnet, expected_addr_sub, 1001, 7201, 7201);
        inner_test(&mut ctx, device, src_ip, dst_ip, subnet, expected_addr_sub, 1001, 7202, 7202);

        // Make remaining lifetime < 2 hrs.
        assert_eq!(run_for(&mut ctx, Duration::from_secs(1000)), []);

        // If the remaining lifetime is <= 2 hrs & valid lifetime is less than
        // that, don't update valid lifetime.
        inner_test(&mut ctx, device, src_ip, dst_ip, subnet, expected_addr_sub, 1001, 6202, 6202);

        // Increase valid lifetime twice while it is less than 2 hours.
        inner_test(&mut ctx, device, src_ip, dst_ip, subnet, expected_addr_sub, 1001, 6203, 6203);
        inner_test(&mut ctx, device, src_ip, dst_ip, subnet, expected_addr_sub, 1001, 6204, 6204);
    }
}
