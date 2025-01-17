use super::*;

/// Error type for `join_multicast_group`, `leave_multicast_group`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum MulticastError {
    /// The hardware device transmit buffer is full. Try again later.
    Exhausted,
    /// The table of joined multicast groups is already full.
    GroupTableFull,
    /// Cannot join/leave the given multicast group.
    Unaddressable,
}

impl core::fmt::Display for MulticastError {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self {
            MulticastError::Exhausted => write!(f, "Exhausted"),
            MulticastError::GroupTableFull => write!(f, "GroupTableFull"),
            MulticastError::Unaddressable => write!(f, "Unaddressable"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for MulticastError {}

impl Interface {
    /// Add an address to a list of subscribed multicast IP addresses.
    ///
    /// Returns `Ok(announce_sent)` if the address was added successfully, where `announce_sent`
    /// indicates whether an initial immediate announcement has been sent.
    pub fn join_multicast_group<D, T: Into<IpAddress>>(
        &mut self,
        device: &mut D,
        addr: T,
        timestamp: Instant,
    ) -> Result<bool, MulticastError>
    where
        D: Device + ?Sized,
    {
        let addr = addr.into();
        self.inner.now = timestamp;

        let is_not_new = self
            .inner
            .multicast_groups
            .insert(addr, ())
            .map_err(|_| MulticastError::GroupTableFull)?
            .is_some();
        if is_not_new {
            return Ok(false);
        }

        match addr {
            IpAddress::Ipv4(addr) => {
                if let Some(pkt) = self.inner.igmp_report_packet(IgmpVersion::Version2, addr) {
                    // Send initial membership report
                    let tx_token = device
                        .transmit(timestamp)
                        .ok_or(MulticastError::Exhausted)?;

                    // NOTE(unwrap): packet destination is multicast, which is always routable and doesn't require neighbor discovery.
                    self.inner
                        .dispatch_ip(tx_token, PacketMeta::default(), pkt, &mut self.fragmenter)
                        .unwrap();

                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            #[cfg(feature = "proto-ipv6")]
            IpAddress::Ipv6(addr) => {
                // Build report packet containing this new address
                if let Some(pkt) = self.inner.mldv2_report_packet(&[MldAddressRecordRepr::new(
                    MldRecordType::ChangeToInclude,
                    addr,
                )]) {
                    // Send initial membership report
                    let tx_token = device
                        .transmit(timestamp)
                        .ok_or(MulticastError::Exhausted)?;

                    // NOTE(unwrap): packet destination is multicast, which is always routable and doesn't require neighbor discovery.
                    self.inner
                        .dispatch_ip(tx_token, PacketMeta::default(), pkt, &mut self.fragmenter)
                        .unwrap();

                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            #[allow(unreachable_patterns)]
            _ => Err(MulticastError::Unaddressable),
        }
    }

    /// Remove an address from the subscribed multicast IP addresses.
    ///
    /// Returns `Ok(leave_sent)` if the address was removed successfully, where `leave_sent`
    /// indicates whether an immediate leave packet has been sent.
    pub fn leave_multicast_group<D, T: Into<IpAddress>>(
        &mut self,
        device: &mut D,
        addr: T,
        timestamp: Instant,
    ) -> Result<bool, MulticastError>
    where
        D: Device + ?Sized,
    {
        let addr = addr.into();
        self.inner.now = timestamp;
        let was_not_present = self.inner.multicast_groups.remove(&addr).is_none();
        if was_not_present {
            return Ok(false);
        }

        match addr {
            IpAddress::Ipv4(addr) => {
                if let Some(pkt) = self.inner.igmp_leave_packet(addr) {
                    // Send group leave packet
                    let tx_token = device
                        .transmit(timestamp)
                        .ok_or(MulticastError::Exhausted)?;

                    // NOTE(unwrap): packet destination is multicast, which is always routable and doesn't require neighbor discovery.
                    self.inner
                        .dispatch_ip(tx_token, PacketMeta::default(), pkt, &mut self.fragmenter)
                        .unwrap();

                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            #[cfg(feature = "proto-ipv6")]
            IpAddress::Ipv6(addr) => {
                if let Some(pkt) = self.inner.mldv2_report_packet(&[MldAddressRecordRepr::new(
                    MldRecordType::ChangeToExclude,
                    addr,
                )]) {
                    // Send group leave packet
                    let tx_token = device
                        .transmit(timestamp)
                        .ok_or(MulticastError::Exhausted)?;

                    // NOTE(unwrap): packet destination is multicast, which is always routable and doesn't require neighbor discovery.
                    self.inner
                        .dispatch_ip(tx_token, PacketMeta::default(), pkt, &mut self.fragmenter)
                        .unwrap();

                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            #[allow(unreachable_patterns)]
            _ => Err(MulticastError::Unaddressable),
        }
    }

    /// Check whether the interface listens to given destination multicast IP address.
    pub fn has_multicast_group<T: Into<IpAddress>>(&self, addr: T) -> bool {
        self.inner.has_multicast_group(addr)
    }

    /// Depending on `igmp_report_state` and the therein contained
    /// timeouts, send IGMP membership reports.
    pub(crate) fn igmp_egress<D>(&mut self, device: &mut D) -> bool
    where
        D: Device + ?Sized,
    {
        match self.inner.igmp_report_state {
            IgmpReportState::ToSpecificQuery {
                version,
                timeout,
                group,
            } if self.inner.now >= timeout => {
                if let Some(pkt) = self.inner.igmp_report_packet(version, group) {
                    // Send initial membership report
                    if let Some(tx_token) = device.transmit(self.inner.now) {
                        // NOTE(unwrap): packet destination is multicast, which is always routable and doesn't require neighbor discovery.
                        self.inner
                            .dispatch_ip(tx_token, PacketMeta::default(), pkt, &mut self.fragmenter)
                            .unwrap();
                    } else {
                        return false;
                    }
                }

                self.inner.igmp_report_state = IgmpReportState::Inactive;
                true
            }
            IgmpReportState::ToGeneralQuery {
                version,
                timeout,
                interval,
                next_index,
            } if self.inner.now >= timeout => {
                let addr = self
                    .inner
                    .multicast_groups
                    .iter()
                    .filter_map(|(addr, _)| match addr {
                        IpAddress::Ipv4(addr) => Some(*addr),
                        #[allow(unreachable_patterns)]
                        _ => None,
                    })
                    .nth(next_index);

                match addr {
                    Some(addr) => {
                        if let Some(pkt) = self.inner.igmp_report_packet(version, addr) {
                            // Send initial membership report
                            if let Some(tx_token) = device.transmit(self.inner.now) {
                                // NOTE(unwrap): packet destination is multicast, which is always routable and doesn't require neighbor discovery.
                                self.inner
                                    .dispatch_ip(
                                        tx_token,
                                        PacketMeta::default(),
                                        pkt,
                                        &mut self.fragmenter,
                                    )
                                    .unwrap();
                            } else {
                                return false;
                            }
                        }

                        let next_timeout = (timeout + interval).max(self.inner.now);
                        self.inner.igmp_report_state = IgmpReportState::ToGeneralQuery {
                            version,
                            timeout: next_timeout,
                            interval,
                            next_index: next_index + 1,
                        };
                        true
                    }

                    None => {
                        self.inner.igmp_report_state = IgmpReportState::Inactive;
                        false
                    }
                }
            }
            _ => false,
        }
    }
}

impl InterfaceInner {
    /// Host duties of the **IGMPv2** protocol.
    ///
    /// Sets up `igmp_report_state` for responding to IGMP general/specific membership queries.
    /// Membership must not be reported immediately in order to avoid flooding the network
    /// after a query is broadcasted by a router; this is not currently done.
    pub(super) fn process_igmp<'frame>(
        &mut self,
        ipv4_repr: Ipv4Repr,
        ip_payload: &'frame [u8],
    ) -> Option<Packet<'frame>> {
        let igmp_packet = check!(IgmpPacket::new_checked(ip_payload));
        let igmp_repr = check!(IgmpRepr::parse(&igmp_packet));

        // FIXME: report membership after a delay
        match igmp_repr {
            IgmpRepr::MembershipQuery {
                group_addr,
                version,
                max_resp_time,
            } => {
                // General query
                if group_addr.is_unspecified()
                    && ipv4_repr.dst_addr == Ipv4Address::MULTICAST_ALL_SYSTEMS
                {
                    let ipv4_multicast_group_count = self
                        .multicast_groups
                        .keys()
                        .filter(|a| matches!(a, IpAddress::Ipv4(_)))
                        .count();

                    // Are we member in any groups?
                    if ipv4_multicast_group_count != 0 {
                        let interval = match version {
                            IgmpVersion::Version1 => Duration::from_millis(100),
                            IgmpVersion::Version2 => {
                                // No dependence on a random generator
                                // (see [#24](https://github.com/m-labs/smoltcp/issues/24))
                                // but at least spread reports evenly across max_resp_time.
                                let intervals = ipv4_multicast_group_count as u32 + 1;
                                max_resp_time / intervals
                            }
                        };
                        self.igmp_report_state = IgmpReportState::ToGeneralQuery {
                            version,
                            timeout: self.now + interval,
                            interval,
                            next_index: 0,
                        };
                    }
                } else {
                    // Group-specific query
                    if self.has_multicast_group(group_addr) && ipv4_repr.dst_addr == group_addr {
                        // Don't respond immediately
                        let timeout = max_resp_time / 4;
                        self.igmp_report_state = IgmpReportState::ToSpecificQuery {
                            version,
                            timeout: self.now + timeout,
                            group: group_addr,
                        };
                    }
                }
            }
            // Ignore membership reports
            IgmpRepr::MembershipReport { .. } => (),
            // Ignore hosts leaving groups
            IgmpRepr::LeaveGroup { .. } => (),
        }

        None
    }
}
