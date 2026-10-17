//! Network choices and traffic counters from a single procfs snapshot.
use super::files::read_trimmed;
use std::{collections::HashSet, fs, path::Path, time::Instant};

#[derive(Clone)]
pub(super) struct NetworkCounters {
    pub(super) read_at: Instant,
    pub(super) received: u64,
    pub(super) transmitted: u64,
    pub(super) interfaces: Vec<String>,
}

pub(super) fn read_network_snapshot(
    selected_interface: Option<&str>,
    sample_counters: bool,
) -> (Vec<String>, Option<NetworkCounters>) {
    let content = fs::read_to_string("/proc/net/dev").unwrap_or_default();
    let choices = content
        .lines()
        .skip(2)
        .filter_map(|line| line.split_once(':').map(|(name, _)| name.trim().to_owned()))
        .filter(|name| name != "lo")
        .collect();
    let counters = sample_counters
        .then(|| {
            // An explicit selection does not need routing table reads.
            let defaults = if selected_interface.is_some() {
                HashSet::new()
            } else {
                read_default_interfaces()
            };
            parse_network_counters(
                &content,
                selected_interface,
                &defaults,
                network_interface_is_active,
            )
        })
        .flatten();
    (choices, counters)
}

pub(super) fn parse_network_counters(
    content: &str,
    selected_interface: Option<&str>,
    default_interfaces: &HashSet<String>,
    is_active: impl Fn(&str) -> bool,
) -> Option<NetworkCounters> {
    let mut candidates = Vec::new();
    for line in content.lines().skip(2) {
        let Some((name, values)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        if name == "lo" {
            continue;
        }
        let mut fields = values.split_whitespace();
        let Some(received) = fields.next().and_then(|value| value.parse::<u64>().ok()) else {
            continue;
        };
        let transmitted = fields.nth(7).and_then(|value| value.parse::<u64>().ok());
        let Some(transmitted) = transmitted else {
            continue;
        };
        if fields.nth(6).is_none() {
            continue;
        }
        candidates.push((name.to_owned(), received, transmitted));
    }

    let use_defaults = selected_interface.is_none()
        && candidates
            .iter()
            .any(|(name, _, _)| default_interfaces.contains(name));
    let mut counters = NetworkCounters {
        read_at: Instant::now(),
        received: 0,
        transmitted: 0,
        interfaces: Vec::new(),
    };
    for (name, received, transmitted) in candidates {
        let selected = if let Some(selected) = selected_interface {
            name == selected && is_active(&name)
        } else if use_defaults {
            default_interfaces.contains(&name)
        } else {
            is_active(&name)
        };
        if selected {
            counters.received = counters.received.saturating_add(received);
            counters.transmitted = counters.transmitted.saturating_add(transmitted);
            counters.interfaces.push(name);
        }
    }
    (!counters.interfaces.is_empty()).then_some(counters)
}

fn read_default_interfaces() -> HashSet<String> {
    parse_default_interfaces(
        &fs::read_to_string("/proc/net/route").unwrap_or_default(),
        &fs::read_to_string("/proc/net/ipv6_route").unwrap_or_default(),
    )
}

pub(super) fn parse_default_interfaces(ipv4: &str, ipv6: &str) -> HashSet<String> {
    let mut interfaces = HashSet::new();
    for line in ipv4.lines().skip(1) {
        let mut fields = line.split_whitespace();
        if let (Some(name), Some("00000000"), Some(_), Some(_)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        {
            interfaces.insert(name.to_owned());
        }
    }
    for line in ipv6.lines() {
        let mut fields = line.split_whitespace();
        if fields.next() == Some("00000000000000000000000000000000")
            && fields.next() == Some("00")
            && let Some(name) = fields.nth(7)
            && name != "lo"
        {
            interfaces.insert(name.to_owned());
        }
    }
    interfaces
}

fn network_interface_is_active(name: &str) -> bool {
    let path = Path::new("/sys/class/net").join(name);
    let state = read_trimmed(path.join("operstate"));
    matches!(state.as_deref(), Some("up") | Some("unknown"))
        || read_trimmed(path.join("carrier")).as_deref() == Some("1")
}

pub(super) fn network_rates(
    previous: &NetworkCounters,
    current: &NetworkCounters,
) -> Option<(f64, f64)> {
    if previous.interfaces != current.interfaces {
        return None;
    }
    let seconds = current
        .read_at
        .duration_since(previous.read_at)
        .as_secs_f64();
    if seconds <= f64::EPSILON {
        return None;
    }
    Some((
        current.received.saturating_sub(previous.received) as f64 / seconds,
        current.transmitted.saturating_sub(previous.transmitted) as f64 / seconds,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_skip_malformed_records_and_saturate_totals() {
        let content = format!(
            "header\nheader\nlo: 20 0 0 0 0 0 0 0 30 0 0 0 0 0 0 0\n\
             empty:\nshort: 9 0 0\nbad: x 0 0 0 0 0 0 0 8 0 0 0 0 0 0 0\n\
             eth0: {} 0 0 0 0 0 0 0 200 0 0 0 0 0 0 0\n\
             wlan0: 5 0 0 0 0 0 0 0 300 0 0 0 0 0 0 0\n",
            u64::MAX
        );
        let counters = parse_network_counters(&content, None, &HashSet::new(), |_| true).unwrap();
        assert_eq!(counters.interfaces, ["eth0", "wlan0"]);
        assert_eq!(counters.received, u64::MAX);
        assert_eq!(counters.transmitted, 500);
        let defaults = HashSet::from(["wlan0".to_owned()]);
        let routed = parse_network_counters(&content, None, &defaults, |_| {
            panic!("routing selection should not probe interface state")
        })
        .unwrap();
        assert_eq!(routed.interfaces, ["wlan0"]);
        assert_eq!(routed.received, 5);
    }

    #[test]
    fn route_parser_ignores_partial_records_and_non_default_routes() {
        let ipv4 = "header\neth0 00000000 0\neth1 00000001 0 3\neth2 00000000 0 3\n";
        let ipv6 = "00000000000000000000000000000000 00 0 0 0 0 0 0 0 lo\n\
                    00000000000000000000000000000000 00 0 0 0 0 0 0 0 wlan0\n\
                    00000000000000000000000000000000 00 0\n";
        assert_eq!(
            parse_default_interfaces(ipv4, ipv6),
            HashSet::from(["eth2".to_owned(), "wlan0".to_owned()])
        );
    }
}
