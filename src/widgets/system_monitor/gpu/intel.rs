use super::*;

#[derive(Clone, Debug, Default)]
struct Client {
    id: String,
    pci: String,
    ns: HashMap<String, u64>,
    cycles: HashMap<String, u64>,
    total_cycles: HashMap<String, u64>,
    capacity: HashMap<String, u64>,
    memory: Option<u64>,
}

fn parse_client(text: &str) -> Option<Client> {
    let mut client = Client::default();
    let mut driver = "";
    let mut resident = HashMap::new();
    let mut total = HashMap::new();
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key {
            "drm-driver" => driver = value,
            "drm-client-id" => client.id = value.into(),
            "drm-pdev" => client.pci = value.to_ascii_lowercase(),
            _ => {
                let mut fields = value.split_whitespace();
                let Some(number) = fields.next().and_then(|number| number.parse::<u64>().ok())
                else {
                    continue;
                };
                let number = number.saturating_mul(match fields.next() {
                    Some("KiB") => 1024,
                    Some("MiB") => 1024 * 1024,
                    _ => 1,
                });
                if let Some(engine) = key.strip_prefix("drm-engine-capacity-") {
                    client.capacity.insert(engine.into(), number.max(1));
                } else if let Some(engine) = key.strip_prefix("drm-engine-") {
                    client.ns.insert(engine.into(), number);
                } else if let Some(engine) = key.strip_prefix("drm-total-cycles-") {
                    client.total_cycles.insert(engine.into(), number);
                } else if let Some(engine) = key.strip_prefix("drm-cycles-") {
                    client.cycles.insert(engine.into(), number);
                } else if let Some(region) = key.strip_prefix("drm-resident-") {
                    resident.insert(region.to_owned(), number);
                } else if let Some(region) = key.strip_prefix("drm-total-") {
                    total.insert(region.to_owned(), number);
                }
            }
        }
    }
    if !matches!(driver, "i915" | "xe") || client.id.is_empty() || client.pci.is_empty() {
        return None;
    }
    for (region, bytes) in resident {
        total.insert(region, bytes);
    }
    client.memory = (!total.is_empty()).then(|| {
        total
            .values()
            .fold(0_u64, |sum, bytes| sum.saturating_add(*bytes))
    });
    Some(client)
}

#[derive(Default)]
pub(super) struct IntelSampler {
    previous: HashMap<String, (Instant, HashMap<String, Client>)>,
}

impl IntelSampler {
    pub(super) fn pause(&mut self, pci: &str) {
        self.previous.remove(pci);
    }

    pub(super) fn sample(
        &mut self,
        card: &Path,
        device: &Path,
        pci: &str,
        request: GpuRequest,
    ) -> GpuSnapshot {
        let mut data = GpuSnapshot {
            client_memory: true,
            ..GpuSnapshot::default()
        };
        if request.load || request.memory {
            let (clients, denied) = read_clients(pci);
            let now = Instant::now();
            let mut clients = clients
                .into_iter()
                .map(|client| (client.id.clone(), client))
                .collect::<HashMap<_, _>>();
            if request.load {
                data.utilization_percent = self.previous.get(pci).and_then(|(time, previous)| {
                    utilization(previous, &mut clients, now.duration_since(*time))
                });
                if data.utilization_percent.is_none() {
                    data.states.insert(
                        "load",
                        if clients.is_empty() {
                            if denied {
                                DataState::PermissionDenied
                            } else {
                                DataState::Unsupported
                            }
                        } else {
                            DataState::Waiting
                        },
                    );
                }
            } else {
                data.states.insert("load", DataState::Paused);
            }
            if request.memory {
                data.memory_used = clients
                    .values()
                    .filter_map(|client| client.memory)
                    .reduce(u64::saturating_add);
                if data.memory_used.is_none() {
                    data.states.insert(
                        "memory-used",
                        if denied {
                            DataState::PermissionDenied
                        } else {
                            DataState::Unsupported
                        },
                    );
                }
                // Client allocations are not the device's physical VRAM capacity.
                data.memory_total = read_u64(device.join("mem_info_vram_total"));
                if data.memory_total.is_none() {
                    data.states.insert("memory-total", DataState::Unsupported);
                }
            } else {
                for key in ["memory-used", "memory-total"] {
                    data.states.insert(key, DataState::Paused);
                }
            }
            self.previous.insert(pci.to_owned(), (now, clients));
        } else {
            self.previous.remove(pci);
            for key in ["load", "memory-used", "memory-total"] {
                data.states.insert(key, DataState::Paused);
            }
        }
        if request.frequency {
            let mut paths = vec![card.join("gt_cur_freq_mhz"), card.join("gt_act_freq_mhz")];
            for root in [card.join("gt"), device.join("gt")] {
                for gt in sorted_directory_paths(&root) {
                    paths.push(gt.join("rps_act_freq_mhz"));
                    paths.push(gt.join("rps_cur_freq_mhz"));
                }
            }
            for tile in sorted_directory_paths(device).into_iter().filter(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("tile"))
            }) {
                for gt in sorted_directory_paths(&tile).into_iter().filter(|path| {
                    path.file_name()
                        .is_some_and(|name| name.to_string_lossy().starts_with("gt"))
                }) {
                    paths.push(gt.join("freq0/act_freq"));
                    paths.push(gt.join("freq0/cur_freq"));
                }
            }
            let mut state = DataState::Unsupported;
            for path in paths {
                match read_value(&path) {
                    Ok(value) => {
                        data.frequency_mhz = Some(value);
                        break;
                    }
                    Err(DataState::PermissionDenied) => state = DataState::PermissionDenied,
                    _ => {}
                }
            }
            if data.frequency_mhz.is_none() {
                data.states.insert("frequency", state);
            }
        } else {
            data.states.insert("frequency", DataState::Paused);
        }
        read_hwmon(device, request, &mut data);
        data
    }
}

// Reading fdinfo does not open a GPU device or create a context. i915 reports
// engine nanoseconds; xe reports busy/total GPU cycles. Both are documented at
// https://docs.kernel.org/gpu/drm-usage-stats.html
fn read_clients(pci: &str) -> (Vec<Client>, bool) {
    let mut clients = HashMap::new();
    let mut denied = false;
    for process in sorted_directory_paths(Path::new("/proc")) {
        if !process
            .file_name()
            .is_some_and(|name| name.to_string_lossy().chars().all(|c| c.is_ascii_digit()))
        {
            continue;
        }
        let Ok(fds) = fs::read_dir(process.join("fd")) else {
            continue;
        };
        for fd in fds.filter_map(Result::ok) {
            let Ok(target) = fs::read_link(fd.path()) else {
                continue;
            };
            if !target.starts_with("/dev/dri") {
                continue;
            }
            match fs::read_to_string(process.join("fdinfo").join(fd.file_name())) {
                Ok(text) => {
                    if let Some(client) = parse_client(&text)
                        && client.pci == pci
                    {
                        clients.entry(client.id.clone()).or_insert(client);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => denied = true,
                _ => {}
            }
        }
    }
    (clients.into_values().collect(), denied)
}

fn utilization(
    previous: &HashMap<String, Client>,
    current: &mut HashMap<String, Client>,
    elapsed: Duration,
) -> Option<f64> {
    let mut engines = HashMap::<String, f64>::new();
    for (id, client) in current {
        let Some(old) = previous.get(id) else {
            continue;
        };
        for (engine, busy) in &mut client.ns {
            let Some(before) = old.ns.get(engine) else {
                continue;
            };
            *busy = (*busy).max(*before);
            let capacity = client.capacity.get(engine).copied().unwrap_or(1).max(1);
            if !elapsed.is_zero() {
                *engines.entry(engine.clone()).or_default() +=
                    (*busy - before) as f64 / elapsed.as_nanos() as f64 / capacity as f64;
            }
        }
        for (engine, busy) in &mut client.cycles {
            if client.ns.contains_key(engine) {
                continue;
            }
            let Some((before, total_before)) =
                old.cycles.get(engine).zip(old.total_cycles.get(engine))
            else {
                continue;
            };
            let Some(total) = client.total_cycles.get_mut(engine) else {
                continue;
            };
            *busy = (*busy).max(*before);
            *total = (*total).max(*total_before);
            let capacity = client.capacity.get(engine).copied().unwrap_or(1).max(1);
            if *total > *total_before {
                *engines.entry(engine.clone()).or_default() +=
                    (*busy - before) as f64 / (*total - total_before) as f64 / capacity as f64;
            }
        }
    }
    engines
        .values()
        .copied()
        .reduce(f64::max)
        .map(|fraction| (fraction * 100.0).clamp(0.0, 100.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn i915_and_xe_use_their_own_counter_units() {
        for (driver, before, after) in [
            (
                "i915",
                "drm-engine-render: 100 ns\ndrm-resident-system: 2 MiB\n",
                "drm-engine-render: 500000100 ns\ndrm-resident-system: 3 MiB\n",
            ),
            (
                "xe",
                "drm-cycles-rcs: 100\ndrm-total-cycles-rcs: 1000\ndrm-engine-capacity-rcs: 2\n",
                "drm-cycles-rcs: 1100\ndrm-total-cycles-rcs: 2000\ndrm-engine-capacity-rcs: 2\n",
            ),
        ] {
            let header =
                format!("drm-driver: {driver}\ndrm-client-id: 3\ndrm-pdev: 0000:00:02.0\n");
            let old = HashMap::from([(
                "3".into(),
                parse_client(&format!("{header}{before}")).unwrap(),
            )]);
            let mut new = HashMap::from([(
                "3".into(),
                parse_client(&format!("{header}{after}")).unwrap(),
            )]);
            assert_eq!(
                utilization(&old, &mut new, Duration::from_secs(1)),
                Some(50.0)
            );
            assert!(parse_client("drm-driver: i915\ndrm-client-id: 3").is_none());
        }
    }

    #[test]
    fn nonmonotonic_engine_counters_do_not_spike_on_recovery() {
        let make = |busy| {
            parse_client(&format!("drm-driver: i915\ndrm-client-id: 9\ndrm-pdev: 0000:00:02.0\ndrm-engine-render: {busy} ns\n")).unwrap()
        };
        let old = HashMap::from([("9".into(), make(500_000_000))]);
        let mut rollback = HashMap::from([("9".into(), make(100_000_000))]);
        assert_eq!(
            utilization(&old, &mut rollback, Duration::from_secs(1)),
            Some(0.0)
        );
        let mut recovered = HashMap::from([("9".into(), make(600_000_000))]);
        assert_eq!(
            utilization(&rollback, &mut recovered, Duration::from_secs(1)),
            Some(10.0)
        );
    }

    #[test]
    fn memory_regions_do_not_count_total_and_resident_twice() {
        let client = parse_client("drm-driver: i915\ndrm-client-id: 9\ndrm-pdev: 0000:00:02.0\ndrm-total-system: 10 MiB\ndrm-resident-system: 2 MiB\ndrm-resident-local0: 512 KiB\n").unwrap();
        assert_eq!(client.memory, Some(2 * 1024 * 1024 + 512 * 1024));
    }
}
