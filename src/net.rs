//! This machine's addresses: which ones count as "the host itself", and which URLs other
//! devices on the LAN should use. Re-scanned periodically because Wi-Fi addresses change.

use std::net::{IpAddr, Ipv4Addr};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct NetInfo {
    pub local_ips: Vec<IpAddr>,
    /// Best first: interfaces carrying the default route (normally the Wi-Fi/Ethernet).
    pub join_urls: Vec<String>,
}

pub fn scan(port: u16) -> NetInfo {
    let ifaces = if_addrs::get_if_addrs().unwrap_or_default();
    let default_ifaces = default_route_ifaces();
    let local_ips = ifaces.iter().map(|i| i.ip()).collect();

    let mut lan: Vec<(bool, String, Ipv4Addr)> = ifaces
        .iter()
        .filter(|i| !i.is_loopback() && !i.is_p2p && !is_virtual(&i.name))
        .filter_map(|i| match &i.addr {
            // A /32 is a point-to-point tunnel (VPNs like Cloudflare WARP), not a LAN.
            if_addrs::IfAddr::V4(v4) if !v4.ip.is_link_local() && v4.netmask != Ipv4Addr::BROADCAST => {
                Some((default_ifaces.contains(&i.name), i.name.clone(), v4.ip))
            }
            _ => None,
        })
        .collect();
    lan.sort_by_key(|(is_default, name, _)| (!is_default, name.clone()));
    let mut join_urls: Vec<String> = lan.iter().map(|(_, _, ip)| format!("http://{ip}:{port}/")).collect();
    join_urls.dedup();
    NetInfo { local_ips, join_urls }
}

fn is_virtual(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    ["docker", "br-", "veth", "virbr", "vmnet", "vboxnet", "tun", "tap", "wg", "tailscale", "zt"]
        .iter()
        .any(|p| name.starts_with(p))
        || name.contains("warp")
}

/// Interfaces with a default route, from /proc/net/route (Destination 00000000).
fn default_route_ifaces() -> Vec<String> {
    let table = std::fs::read_to_string("/proc/net/route").unwrap_or_default();
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let mut cols = line.split_whitespace();
            let iface = cols.next()?;
            (cols.next()? == "00000000").then(|| iface.to_owned())
        })
        .collect()
}
