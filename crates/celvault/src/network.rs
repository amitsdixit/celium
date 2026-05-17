//! Per-node networking primitives.
//!
//! Week-15 introduces a *control-plane* model for virtual networks,
//! security groups, and load balancers. The data plane is **not**
//! implemented in this crate — actual packet forwarding happens
//! later in CelHyper. What we provide here is the policy and
//! topology surface that the rest of Celium (and the K8s
//! personality) needs to reason about:
//!
//! * [`VirtualNetwork`] — a logical L3 broadcast domain with a
//!   private CIDR block. Each network is owned by exactly one node;
//!   federation is layered on top via gossip the same way volumes
//!   are.
//! * [`Nic`] — one network interface attached to a VM. Allocated
//!   inside the network's CIDR using a deterministic linear
//!   allocator so a recreated VM ends up with a deterministic IP
//!   for the same `vm_id`.
//! * [`SecurityGroup`] / [`SecurityRule`] — declarative ACL applied
//!   to a `Nic` or to a whole VM. Stateless, ingress + egress.
//! * [`LoadBalancer`] — VIP → backend table. Round-robin or
//!   least-connection distribution. Backends reference VM ids and
//!   ports.
//!
//! The trait surface mirrors [`crate::VolumeStore`]: synchronous
//! `&self` calls, internal locking, every fallible call returns
//! `CelResult<T>`. A reference [`MemNetworkStore`] is provided for
//! tests and the W15 in-process demo. A future disk-backed store
//! can layer on the same trait.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::sync::Mutex;

use celcommon::{CelError, CelResult};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------------

/// Globally-unique network identifier. Wire form: `"<owner>/n<n>"`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct NetworkId(pub String);

impl NetworkId {
    /// Borrow the underlying string slice.
    #[must_use]
    pub fn as_str(&self) -> &str { &self.0 }
}

impl std::fmt::Display for NetworkId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for NetworkId {
    fn from(s: &str) -> Self { Self(s.to_string()) }
}

/// Globally-unique NIC identifier. Wire form: `"<network>/nic<n>"`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct NicId(pub String);

impl NicId {
    /// Borrow the underlying string slice.
    #[must_use]
    pub fn as_str(&self) -> &str { &self.0 }
}

impl std::fmt::Display for NicId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for NicId {
    fn from(s: &str) -> Self { Self(s.to_string()) }
}

/// Globally-unique security group id. Wire form: `"<owner>/sg<n>"`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SecurityGroupId(pub String);

impl SecurityGroupId {
    /// Borrow the underlying string slice.
    #[must_use]
    pub fn as_str(&self) -> &str { &self.0 }
}

impl std::fmt::Display for SecurityGroupId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for SecurityGroupId {
    fn from(s: &str) -> Self { Self(s.to_string()) }
}

/// Globally-unique load-balancer id. Wire form: `"<owner>/lb<n>"`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct LoadBalancerId(pub String);

impl LoadBalancerId {
    /// Borrow the underlying string slice.
    #[must_use]
    pub fn as_str(&self) -> &str { &self.0 }
}

impl std::fmt::Display for LoadBalancerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for LoadBalancerId {
    fn from(s: &str) -> Self { Self(s.to_string()) }
}

// ---------------------------------------------------------------------------
// Caps
// ---------------------------------------------------------------------------

/// Maximum permitted name length on any network/sg/lb resource.
pub const MAX_NAME: usize = 64;

/// Maximum NICs per network. Bounds the linear IP allocator so an
/// adversary cannot force unbounded scans.
pub const MAX_NICS_PER_NETWORK: u32 = 256;

/// Maximum security rules per group. Stops a peer from blowing up
/// the wire frame budget.
pub const MAX_RULES_PER_GROUP: usize = 32;

/// Maximum backends per load balancer.
pub const MAX_BACKENDS_PER_LB: usize = 32;

// ---------------------------------------------------------------------------
// Records
// ---------------------------------------------------------------------------

/// IPv4 CIDR block. Stored decoded (`base`/`prefix_len`) so the
/// allocator never re-parses on the hot path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Cidr4 {
    /// Network base address (already masked to `prefix_len`).
    pub base: Ipv4Addr,
    /// Mask length. Must be in `0..=32`; the allocator rejects `/0`
    /// and `/32` because they are degenerate.
    pub prefix_len: u8,
}

impl Cidr4 {
    /// Parse a CIDR string of the form `"10.0.0.0/24"`.
    ///
    /// # Errors
    /// Returns `CelError::Invalid` for malformed input or out-of-range
    /// prefixes.
    pub fn parse(s: &str) -> CelResult<Self> {
        let (ip, p) = s.split_once('/').ok_or(CelError::Invalid("cidr: missing '/'"))?;
        let ip: Ipv4Addr = ip.parse().map_err(|_| CelError::Invalid("cidr: bad ipv4"))?;
        let prefix_len: u8 = p.parse().map_err(|_| CelError::Invalid("cidr: bad prefix"))?;
        if prefix_len == 0 || prefix_len >= 32 {
            return Err(CelError::Invalid("cidr: prefix must be in 1..32"));
        }
        let mask = !0u32 << (32 - prefix_len);
        let base = u32::from(ip) & mask;
        Ok(Self { base: Ipv4Addr::from(base), prefix_len })
    }

    /// Total addressable host count (excluding network + broadcast).
    #[must_use]
    pub fn capacity(&self) -> u32 {
        let host_bits = 32u32 - u32::from(self.prefix_len);
        // host_bits in 1..32 here, so the shift is safe.
        (1u32 << host_bits).saturating_sub(2)
    }

    /// `n`-th usable host address (1-based, skips network + broadcast).
    /// Returns `None` if `n` is out of range.
    #[must_use]
    pub fn nth_host(&self, n: u32) -> Option<Ipv4Addr> {
        if n == 0 || n > self.capacity() { return None; }
        Some(Ipv4Addr::from(u32::from(self.base) + n))
    }

    /// `true` if `addr` falls inside this block.
    #[must_use]
    pub fn contains(&self, addr: Ipv4Addr) -> bool {
        let mask = if self.prefix_len == 0 { 0 }
                   else { !0u32 << (32 - self.prefix_len) };
        (u32::from(addr) & mask) == u32::from(self.base)
    }
}

impl std::fmt::Display for Cidr4 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.base, self.prefix_len)
    }
}

/// Virtual network metadata. The actual packet plane is owned by
/// CelHyper; CelVault just tracks topology + policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VirtualNetwork {
    /// Globally-unique id.
    pub id: NetworkId,
    /// Owning node.
    pub owner: String,
    /// Free-form name (≤ [`MAX_NAME`]).
    pub name: String,
    /// Address block.
    pub cidr: Cidr4,
}

/// One NIC attached to a VM on a network.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Nic {
    /// Globally-unique id.
    pub id: NicId,
    /// Network this NIC lives on.
    pub network_id: NetworkId,
    /// VM the NIC is attached to.
    pub vm_id: u32,
    /// Allocated IPv4 address.
    pub ip: Ipv4Addr,
    /// MAC address. Locally-administered, generated from `(network, idx)`
    /// so it's deterministic for the same NIC ordinal.
    pub mac: [u8; 6],
}

/// Direction of a security rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// Inbound — match traffic destined to the protected NIC/VM.
    Ingress,
    /// Outbound — match traffic originating from the protected NIC/VM.
    Egress,
}

/// L4 protocol for a security rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum L4Proto {
    /// TCP.
    Tcp,
    /// UDP.
    Udp,
    /// ICMPv4.
    Icmp,
    /// Match every protocol.
    Any,
}

/// One declarative ACL row. Stateless. Empty rule sets default-deny
/// in W15; CelHyper will enforce the actual filter later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecurityRule {
    /// Direction the rule applies to.
    pub direction: Direction,
    /// Protocol filter.
    pub proto: L4Proto,
    /// Inclusive port range. For [`L4Proto::Icmp`] / [`L4Proto::Any`]
    /// the range is ignored.
    pub port_min: u16,
    /// Inclusive port range upper bound.
    pub port_max: u16,
    /// Source (Ingress) or destination (Egress) CIDR.
    pub cidr: Cidr4,
    /// `true` for `allow`, `false` for `deny`. Order matters: rules
    /// are evaluated top-to-bottom, first match wins.
    pub allow: bool,
}

/// A named, owner-scoped bag of [`SecurityRule`]s. Applied to a NIC
/// (or whole VM) by id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecurityGroup {
    /// Globally-unique id.
    pub id: SecurityGroupId,
    /// Owning node.
    pub owner: String,
    /// Free-form name (≤ [`MAX_NAME`]).
    pub name: String,
    /// Ordered rule list.
    pub rules: Vec<SecurityRule>,
}

/// Load-balancing algorithm. We deliberately keep this small — more
/// can be added without a wire bump because the tag is `snake_case`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LbAlgo {
    /// Cycle through backends in declared order.
    #[default]
    RoundRobin,
    /// Pick the backend with the fewest active connections. The
    /// connection count is supplied at dispatch time.
    LeastConn,
}

/// One backend behind a load balancer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LbBackend {
    /// VM id on the LB owner's node.
    pub vm_id: u32,
    /// Backend address (must live on the LB's network).
    pub ip: Ipv4Addr,
    /// Backend port.
    pub port: u16,
}

/// Full load-balancer record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoadBalancer {
    /// Globally-unique id.
    pub id: LoadBalancerId,
    /// Owning node.
    pub owner: String,
    /// Free-form name (≤ [`MAX_NAME`]).
    pub name: String,
    /// Network this LB lives on.
    pub network_id: NetworkId,
    /// Front-end VIP (must lie inside the network's CIDR).
    pub vip: Ipv4Addr,
    /// Front-end port.
    pub frontend_port: u16,
    /// Distribution policy.
    pub algo: LbAlgo,
    /// Ordered backend list.
    pub backends: Vec<LbBackend>,
}

impl LoadBalancer {
    /// Pure dispatch helper — selects the next backend index.
    ///
    /// `tick` is a monotonically-increasing counter the caller uses
    /// for round-robin; `least_conn` is the per-backend connection
    /// count used by [`LbAlgo::LeastConn`]. Returns `None` if the LB
    /// has no backends.
    #[must_use]
    pub fn pick(&self, tick: u64, least_conn: &[u32]) -> Option<usize> {
        if self.backends.is_empty() { return None; }
        match self.algo {
            LbAlgo::RoundRobin => Some((tick % self.backends.len() as u64) as usize),
            LbAlgo::LeastConn  => {
                let mut best = 0usize;
                let mut best_n = u32::MAX;
                for (i, _) in self.backends.iter().enumerate() {
                    let n = least_conn.get(i).copied().unwrap_or(0);
                    if n < best_n { best_n = n; best = i; }
                }
                Some(best)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// Per-node networking control-plane store.
pub trait NetworkStore: Send + Sync {
    // -- Networks
    /// Create a network owned by `owner`.
    fn create_network(&self, owner: &str, name: &str, cidr: Cidr4) -> CelResult<VirtualNetwork>;
    /// Delete a network. Idempotent. Errors if any NIC is still attached.
    fn delete_network(&self, id: &NetworkId) -> CelResult<()>;
    /// All known networks.
    fn list_networks(&self) -> Vec<VirtualNetwork>;
    /// Lookup a network.
    fn get_network(&self, id: &NetworkId) -> Option<VirtualNetwork>;

    // -- NICs
    /// Allocate a NIC for `vm_id` on `network_id`. The IP is picked
    /// linearly inside the network's CIDR; callers may pass `Some`
    /// to request a specific address (must be free).
    fn attach_nic(
        &self,
        network_id: &NetworkId,
        vm_id: u32,
        ip: Option<Ipv4Addr>,
    ) -> CelResult<Nic>;
    /// Detach a NIC. Idempotent.
    fn detach_nic(&self, id: &NicId) -> CelResult<()>;
    /// Every NIC in the store.
    fn list_nics(&self) -> Vec<Nic>;

    // -- Security groups
    /// Create a security group with the given (validated) rules.
    fn create_security_group(
        &self,
        owner: &str,
        name: &str,
        rules: Vec<SecurityRule>,
    ) -> CelResult<SecurityGroup>;
    /// Delete a security group. Idempotent.
    fn delete_security_group(&self, id: &SecurityGroupId) -> CelResult<()>;
    /// All known groups.
    fn list_security_groups(&self) -> Vec<SecurityGroup>;

    // -- Load balancers
    /// Create a load balancer with the given backends.
    #[allow(clippy::too_many_arguments)] // Trait shape is part of the public API; refactoring to a builder is W21.
    fn create_load_balancer(
        &self,
        owner: &str,
        name: &str,
        network_id: &NetworkId,
        vip: Ipv4Addr,
        frontend_port: u16,
        algo: LbAlgo,
        backends: Vec<LbBackend>,
    ) -> CelResult<LoadBalancer>;
    /// Delete a load balancer. Idempotent.
    fn delete_load_balancer(&self, id: &LoadBalancerId) -> CelResult<()>;
    /// All known load balancers.
    fn list_load_balancers(&self) -> Vec<LoadBalancer>;
}

// ---------------------------------------------------------------------------
// In-memory implementation
// ---------------------------------------------------------------------------

#[derive(Default)]
struct MemInner {
    next_net: u64,
    next_sg:  u64,
    next_lb:  u64,
    /// Per-network monotonic NIC counter.
    next_nic: BTreeMap<NetworkId, u32>,
    /// Per-network IP allocator bitmap-as-set.
    used_ips: BTreeMap<NetworkId, BTreeMap<Ipv4Addr, NicId>>,
    networks: BTreeMap<NetworkId, VirtualNetwork>,
    nics:     BTreeMap<NicId, Nic>,
    sgs:      BTreeMap<SecurityGroupId, SecurityGroup>,
    lbs:      BTreeMap<LoadBalancerId, LoadBalancer>,
}

/// Reference in-memory implementation of [`NetworkStore`].
pub struct MemNetworkStore {
    inner: Mutex<MemInner>,
}

impl Default for MemNetworkStore {
    fn default() -> Self { Self::new() }
}

impl MemNetworkStore {
    /// Construct an empty store.
    #[must_use]
    pub fn new() -> Self { Self { inner: Mutex::new(MemInner::default()) } }

    fn lock(&self) -> std::sync::MutexGuard<'_, MemInner> {
        // No `unsafe`. A poisoned guard is recovered because no
        // critical section here leaves the structure half-mutated.
        match self.inner.lock() {
            Ok(g)  => g,
            Err(p) => p.into_inner(),
        }
    }

    fn validate_name(name: &str) -> CelResult<()> {
        if name.is_empty()         { return Err(CelError::Invalid("name: empty")); }
        if name.len() > MAX_NAME   { return Err(CelError::Invalid("name: too long")); }
        Ok(())
    }
}

fn make_mac(network: &NetworkId, idx: u32) -> [u8; 6] {
    // Locally-administered, deterministic per (network, idx).
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in network.0.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h ^= u64::from(idx);
    h = h.wrapping_mul(0x100_0000_01b3);
    [
        // 0x02 = locally administered, unicast.
        0x02,
        ((h >> 32) & 0xff) as u8,
        ((h >> 24) & 0xff) as u8,
        ((h >> 16) & 0xff) as u8,
        ((h >> 8)  & 0xff) as u8,
        ( h        & 0xff) as u8,
    ]
}

impl NetworkStore for MemNetworkStore {
    fn create_network(&self, owner: &str, name: &str, cidr: Cidr4) -> CelResult<VirtualNetwork> {
        Self::validate_name(name)?;
        let mut g = self.lock();
        g.next_net = g.next_net.saturating_add(1);
        let id = NetworkId(format!("{owner}/n{}", g.next_net));
        let net = VirtualNetwork {
            id: id.clone(),
            owner: owner.to_string(),
            name: name.to_string(),
            cidr,
        };
        g.networks.insert(id.clone(), net.clone());
        g.next_nic.insert(id.clone(), 0);
        g.used_ips.insert(id, BTreeMap::new());
        Ok(net)
    }

    fn delete_network(&self, id: &NetworkId) -> CelResult<()> {
        let mut g = self.lock();
        if let Some(used) = g.used_ips.get(id) {
            if !used.is_empty() {
                return Err(CelError::Invalid("network: NICs still attached"));
            }
        }
        g.networks.remove(id);
        g.next_nic.remove(id);
        g.used_ips.remove(id);
        // Drop LBs / SG references to this network.
        let drop_lbs: Vec<_> = g.lbs.iter()
            .filter(|(_, lb)| &lb.network_id == id)
            .map(|(k, _)| k.clone()).collect();
        for k in drop_lbs { g.lbs.remove(&k); }
        Ok(())
    }

    fn list_networks(&self) -> Vec<VirtualNetwork> {
        self.lock().networks.values().cloned().collect()
    }

    fn get_network(&self, id: &NetworkId) -> Option<VirtualNetwork> {
        self.lock().networks.get(id).cloned()
    }

    fn attach_nic(
        &self,
        network_id: &NetworkId,
        vm_id: u32,
        ip: Option<Ipv4Addr>,
    ) -> CelResult<Nic> {
        let mut g = self.lock();
        let net = g.networks.get(network_id)
            .ok_or(CelError::Invalid("attach_nic: unknown network"))?
            .clone();
        let used_count = g.used_ips.get(network_id).map(BTreeMap::len).unwrap_or(0);
        if used_count >= MAX_NICS_PER_NETWORK as usize {
            return Err(CelError::Exhausted("attach_nic: per-network NIC cap"));
        }
        let chosen = if let Some(req) = ip {
            if !net.cidr.contains(req) {
                return Err(CelError::Invalid("attach_nic: ip outside network"));
            }
            if g.used_ips.get(network_id).map(|m| m.contains_key(&req)).unwrap_or(false) {
                return Err(CelError::Invalid("attach_nic: ip already in use"));
            }
            req
        } else {
            // Linear scan for the first free host address.
            let used = g.used_ips.get(network_id).cloned().unwrap_or_default();
            let mut found = None;
            for n in 1..=net.cidr.capacity() {
                if let Some(addr) = net.cidr.nth_host(n) {
                    if !used.contains_key(&addr) { found = Some(addr); break; }
                }
            }
            found.ok_or(CelError::Exhausted("attach_nic: CIDR exhausted"))?
        };
        let counter = {
            let c = g.next_nic.entry(network_id.clone()).or_insert(0);
            *c = c.saturating_add(1);
            *c
        };
        let nic_id = NicId(format!("{network_id}/nic{counter}"));
        let nic = Nic {
            id: nic_id.clone(),
            network_id: network_id.clone(),
            vm_id,
            ip: chosen,
            mac: make_mac(network_id, counter),
        };
        g.used_ips.entry(network_id.clone()).or_default().insert(chosen, nic_id.clone());
        g.nics.insert(nic_id, nic.clone());
        Ok(nic)
    }

    fn detach_nic(&self, id: &NicId) -> CelResult<()> {
        let mut g = self.lock();
        if let Some(nic) = g.nics.remove(id) {
            if let Some(map) = g.used_ips.get_mut(&nic.network_id) {
                map.remove(&nic.ip);
            }
        }
        Ok(())
    }

    fn list_nics(&self) -> Vec<Nic> {
        self.lock().nics.values().cloned().collect()
    }

    fn create_security_group(
        &self,
        owner: &str,
        name: &str,
        rules: Vec<SecurityRule>,
    ) -> CelResult<SecurityGroup> {
        Self::validate_name(name)?;
        if rules.len() > MAX_RULES_PER_GROUP {
            return Err(CelError::Invalid("sg: too many rules"));
        }
        for r in &rules {
            if r.port_min > r.port_max {
                return Err(CelError::Invalid("sg: port_min > port_max"));
            }
        }
        let mut g = self.lock();
        g.next_sg = g.next_sg.saturating_add(1);
        let id = SecurityGroupId(format!("{owner}/sg{}", g.next_sg));
        let sg = SecurityGroup {
            id: id.clone(),
            owner: owner.to_string(),
            name: name.to_string(),
            rules,
        };
        g.sgs.insert(id, sg.clone());
        Ok(sg)
    }

    fn delete_security_group(&self, id: &SecurityGroupId) -> CelResult<()> {
        self.lock().sgs.remove(id);
        Ok(())
    }

    fn list_security_groups(&self) -> Vec<SecurityGroup> {
        self.lock().sgs.values().cloned().collect()
    }

    fn create_load_balancer(
        &self,
        owner: &str,
        name: &str,
        network_id: &NetworkId,
        vip: Ipv4Addr,
        frontend_port: u16,
        algo: LbAlgo,
        backends: Vec<LbBackend>,
    ) -> CelResult<LoadBalancer> {
        Self::validate_name(name)?;
        if backends.len() > MAX_BACKENDS_PER_LB {
            return Err(CelError::Invalid("lb: too many backends"));
        }
        let mut g = self.lock();
        let net = g.networks.get(network_id)
            .ok_or(CelError::Invalid("lb: unknown network"))?
            .clone();
        if !net.cidr.contains(vip) {
            return Err(CelError::Invalid("lb: vip outside network"));
        }
        for b in &backends {
            if !net.cidr.contains(b.ip) {
                return Err(CelError::Invalid("lb: backend ip outside network"));
            }
        }
        g.next_lb = g.next_lb.saturating_add(1);
        let id = LoadBalancerId(format!("{owner}/lb{}", g.next_lb));
        let lb = LoadBalancer {
            id: id.clone(),
            owner: owner.to_string(),
            name: name.to_string(),
            network_id: network_id.clone(),
            vip,
            frontend_port,
            algo,
            backends,
        };
        g.lbs.insert(id, lb.clone());
        Ok(lb)
    }

    fn delete_load_balancer(&self, id: &LoadBalancerId) -> CelResult<()> {
        self.lock().lbs.remove(id);
        Ok(())
    }

    fn list_load_balancers(&self) -> Vec<LoadBalancer> {
        self.lock().lbs.values().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cidr_parse_and_alloc() {
        let c = Cidr4::parse("10.42.0.0/24").unwrap();
        assert_eq!(c.capacity(), 254);
        assert_eq!(c.nth_host(1).unwrap(), Ipv4Addr::new(10, 42, 0, 1));
        assert_eq!(c.nth_host(254).unwrap(), Ipv4Addr::new(10, 42, 0, 254));
        assert!(c.nth_host(255).is_none());
        assert!(c.contains(Ipv4Addr::new(10, 42, 0, 100)));
        assert!(!c.contains(Ipv4Addr::new(10, 43, 0, 1)));
    }

    #[test]
    fn cidr_parse_rejects_bad_input() {
        assert!(Cidr4::parse("not-an-ip").is_err());
        assert!(Cidr4::parse("10.0.0.0/0").is_err());
        assert!(Cidr4::parse("10.0.0.0/32").is_err());
    }

    #[test]
    fn networks_nics_round_trip() {
        let s = MemNetworkStore::new();
        let net = s.create_network("n1", "default", Cidr4::parse("10.0.0.0/29").unwrap()).unwrap();
        let n1 = s.attach_nic(&net.id, 0, None).unwrap();
        let n2 = s.attach_nic(&net.id, 1, None).unwrap();
        assert_ne!(n1.ip, n2.ip);
        assert!(net.cidr.contains(n1.ip));
        assert!(net.cidr.contains(n2.ip));
        assert_eq!(s.list_nics().len(), 2);

        // Network deletion blocked by attachments.
        assert!(s.delete_network(&net.id).is_err());
        s.detach_nic(&n1.id).unwrap();
        s.detach_nic(&n2.id).unwrap();
        s.delete_network(&net.id).unwrap();
        assert!(s.list_networks().is_empty());
    }

    #[test]
    fn explicit_ip_request_honoured_then_rejected() {
        let s = MemNetworkStore::new();
        let net = s.create_network("n1", "x", Cidr4::parse("10.0.0.0/29").unwrap()).unwrap();
        let want = Ipv4Addr::new(10, 0, 0, 3);
        let nic = s.attach_nic(&net.id, 0, Some(want)).unwrap();
        assert_eq!(nic.ip, want);
        let err = s.attach_nic(&net.id, 1, Some(want)).unwrap_err();
        assert!(matches!(err, CelError::Invalid(_)));
    }

    #[test]
    fn cidr_exhaustion_is_explicit() {
        let s = MemNetworkStore::new();
        let net = s.create_network("n1", "tiny", Cidr4::parse("10.0.0.0/30").unwrap()).unwrap();
        // /30 has 2 hosts.
        s.attach_nic(&net.id, 0, None).unwrap();
        s.attach_nic(&net.id, 1, None).unwrap();
        let err = s.attach_nic(&net.id, 2, None).unwrap_err();
        assert!(matches!(err, CelError::Exhausted(_)));
    }

    #[test]
    fn security_groups_validate_rules() {
        let s = MemNetworkStore::new();
        let bad = vec![SecurityRule {
            direction: Direction::Ingress,
            proto: L4Proto::Tcp,
            port_min: 100, port_max: 50,
            cidr: Cidr4::parse("10.0.0.0/24").unwrap(),
            allow: true,
        }];
        assert!(s.create_security_group("n1", "bad", bad).is_err());
        let good = vec![SecurityRule {
            direction: Direction::Ingress,
            proto: L4Proto::Tcp,
            port_min: 80, port_max: 80,
            cidr: Cidr4::parse("0.0.0.0/1").unwrap(),
            allow: true,
        }];
        let sg = s.create_security_group("n1", "web", good).unwrap();
        assert_eq!(sg.rules.len(), 1);
        assert_eq!(s.list_security_groups().len(), 1);
    }

    #[test]
    fn load_balancer_round_robin_pick() {
        let s = MemNetworkStore::new();
        let net = s.create_network("n1", "x", Cidr4::parse("10.0.0.0/24").unwrap()).unwrap();
        let backends = vec![
            LbBackend { vm_id: 0, ip: Ipv4Addr::new(10,0,0,10), port: 80 },
            LbBackend { vm_id: 1, ip: Ipv4Addr::new(10,0,0,11), port: 80 },
            LbBackend { vm_id: 2, ip: Ipv4Addr::new(10,0,0,12), port: 80 },
        ];
        let lb = s.create_load_balancer(
            "n1", "web", &net.id,
            Ipv4Addr::new(10,0,0,200), 80, LbAlgo::RoundRobin, backends,
        ).unwrap();
        assert_eq!(lb.pick(0, &[]).unwrap(), 0);
        assert_eq!(lb.pick(1, &[]).unwrap(), 1);
        assert_eq!(lb.pick(2, &[]).unwrap(), 2);
        assert_eq!(lb.pick(3, &[]).unwrap(), 0);
    }

    #[test]
    fn load_balancer_least_conn_pick() {
        let s = MemNetworkStore::new();
        let net = s.create_network("n1", "x", Cidr4::parse("10.0.0.0/24").unwrap()).unwrap();
        let backends = vec![
            LbBackend { vm_id: 0, ip: Ipv4Addr::new(10,0,0,10), port: 80 },
            LbBackend { vm_id: 1, ip: Ipv4Addr::new(10,0,0,11), port: 80 },
        ];
        let lb = s.create_load_balancer(
            "n1", "web", &net.id,
            Ipv4Addr::new(10,0,0,200), 80, LbAlgo::LeastConn, backends,
        ).unwrap();
        assert_eq!(lb.pick(99, &[5, 1]).unwrap(), 1);
        assert_eq!(lb.pick(99, &[2, 7]).unwrap(), 0);
    }
}
